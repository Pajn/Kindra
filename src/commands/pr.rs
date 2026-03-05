use crate::gh::{self, CreatePrParams};
use crate::stack::{
    StackBranch, compute_base_map, get_stack_branches, sort_branches_topologically,
};
use anyhow::{Context, Result};
use clap::Subcommand;
use git2::{BranchType, Repository};
use std::fs;
use std::io::Write;
use tempfile::NamedTempFile;

#[derive(Subcommand, Clone, Copy)]
pub enum PrSubcommand {
    /// Open an existing PR from the current stack in your default browser
    Open,
    /// Edit an existing PR from the current stack
    Edit,
}

pub fn pr(subcommand: &Option<PrSubcommand>) -> Result<()> {
    match subcommand {
        Some(PrSubcommand::Open) => pr_open(),
        Some(PrSubcommand::Edit) => pr_edit(),
        None => pr_create_or_update(),
    }
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

    for (sb, _remote_upstream) in &branches_with_upstream {
        let git_base = base_map
            .get(&sb.name)
            .cloned()
            .unwrap_or_else(|| upstream_name.clone());
        let gh_base = normalize_base_for_gh(&git_base);

        process_branch_pr(&repo, &sb.name, &git_base, &gh_base)?;
        println!();
    }

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

    let selected_index = if prs.len() == 1 {
        0
    } else {
        let options: Vec<String> = prs
            .iter()
            .map(|(branch, pr)| format!("{} → {}", branch, pr.url))
            .collect();
        let selection = crate::commands::prompt_select("Select PR to edit:", options)?;
        prs.iter()
            .position(|(branch, pr)| format!("{} → {}", branch, pr.url) == selection)
            .ok_or_else(|| anyhow::anyhow!("Selected PR not found"))?
    };

    let (branch_name, existing) = &prs[selected_index];
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

    gh::edit_pr(&gh::EditPrParams {
        number: existing.number,
        title,
        body,
        current_labels: existing.labels.clone(),
        labels,
        current_reviewers: existing.reviewers.clone(),
        reviewers,
    })?;
    println!("✓ PR updated: {}", existing.url);
    Ok(())
}

fn discover_stack_branches_with_upstream(
    repo: &Repository,
) -> Result<(String, Vec<(StackBranch, String)>)> {
    let upstream_name = crate::commands::find_upstream(repo)?;
    let upstream_obj = repo.revparse_single(&upstream_name)?;
    let upstream_id = upstream_obj.id();
    let head_id = repo.head()?.peel_to_commit()?.id();

    // Collect all stack branches and sort bottom→top so base branches are
    // processed before the branches that depend on them.
    let mut stack_branches = get_stack_branches(repo, head_id, upstream_id, &upstream_name)?;
    sort_branches_topologically(repo, &mut stack_branches)?;

    if stack_branches.is_empty() {
        println!("No branches in stack.");
        return Ok((upstream_name, Vec::new()));
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

    Ok((upstream_name, branches_with_upstream))
}

// ────────────────────────────────────────────────────────────────────────────
// Per-branch PR logic
// ────────────────────────────────────────────────────────────────────────────

fn normalize_base_for_gh(base: &str) -> String {
    base.rsplit_once('/')
        .map(|(_, short)| short)
        .unwrap_or(base)
        .to_string()
}

fn process_branch_pr(
    repo: &Repository,
    branch_name: &str,
    git_base: &str,
    gh_base: &str,
) -> Result<()> {
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
        }
        None => {
            // New PR: run the interactive wizard
            create_pr_interactive(repo, branch_name, git_base, gh_base)?;
        }
    }

    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// Interactive PR creation wizard
// ────────────────────────────────────────────────────────────────────────────

fn create_pr_interactive(
    repo: &Repository,
    branch_name: &str,
    git_base: &str,
    gh_base: &str,
) -> Result<()> {
    let commits = get_branch_commits(repo, branch_name, git_base)?;

    if commits.is_empty() {
        println!(
            "No commits on this branch compared to '{}'. Skipping.",
            git_base
        );
        return Ok(());
    }

    // ── Step 1: Title ────────────────────────────────────────────────────────
    let title = prompt_title(&commits)?;
    if title.is_empty() {
        println!("  PR title is empty. Skipping {}.", branch_name);
        return Ok(());
    }

    // ── Step 2: Body ─────────────────────────────────────────────────────────
    let body = prompt_body(branch_name, &commits)?;

    // ── Step 3: Submit options ───────────────────────────────────────────────
    let submission = prompt_submit_options()?;

    println!("  Creating PR...");
    let url = gh::create_pr(&CreatePrParams {
        title,
        body,
        base: gh_base.to_string(),
        head: branch_name.to_string(),
        draft: submission.draft,
        labels: submission.labels,
        reviewers: submission.reviewers,
    })?;

    println!("  ✓ PR created: {}", url);
    Ok(())
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
