use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::process::Command;

/// Verify that the `gh` CLI is installed and authenticated.
pub fn check_gh() -> Result<()> {
    let status = Command::new("gh")
        .arg("auth")
        .arg("status")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err(anyhow!(
            "`gh` is not authenticated. Run `gh auth login` first."
        )),
        Err(_) => Err(anyhow!(
            "`gh` CLI not found. Install it from https://cli.github.com/ and run `gh auth login`."
        )),
    }
}

#[derive(Debug, Clone)]
pub struct ExistingPr {
    pub number: u64,
    pub base_branch: String,
}

#[derive(Debug, Clone)]
pub struct OpenPrUrl {
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct EditablePr {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    pub labels: Vec<String>,
    pub reviewers: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ReviewerStatus {
    pub reviewer: String,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct PrStatusSummary {
    pub reviewer_statuses: Vec<ReviewerStatus>,
    pub unresolved_comments: usize,
    pub running_checks: Vec<String>,
    pub failed_checks: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ReviewCommentAuthor {
    pub login: String,
    pub is_bot: bool,
}

#[derive(Debug, Clone)]
pub struct PrReviewComment {
    pub author: ReviewCommentAuthor,
    pub body: String,
    pub path: String,
    pub line: Option<u64>,
    pub start_line: Option<u64>,
    pub original_line: Option<u64>,
    pub original_start_line: Option<u64>,
    pub outdated: bool,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct PrReviewThread {
    pub is_resolved: bool,
    pub comments: Vec<PrReviewComment>,
}

/// Check if an open PR exists for `branch`. Returns `Some(ExistingPr)` or `None`.
pub fn find_open_pr(branch: &str) -> Result<Option<ExistingPr>> {
    #[derive(Deserialize)]
    struct PrView {
        number: u64,
        #[serde(rename = "baseRefName")]
        base_ref_name: String,
        state: String,
    }

    let output = Command::new("gh")
        .args(["pr", "view", branch, "--json", "number,baseRefName,state"])
        .output()
        .context("Failed to run `gh pr view`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no pull requests found for branch") {
            return Ok(None);
        }
        return Err(anyhow!("`gh pr view` failed: {}", stderr.trim()));
    }

    let pr: PrView =
        serde_json::from_slice(&output.stdout).context("Failed to parse `gh pr view` output")?;

    if pr.state.to_uppercase() == "OPEN" {
        Ok(Some(ExistingPr {
            number: pr.number,
            base_branch: pr.base_ref_name,
        }))
    } else {
        Ok(None)
    }
}

/// Check if an open PR exists for `branch`. Returns its URL if open.
pub fn find_open_pr_url(branch: &str) -> Result<Option<OpenPrUrl>> {
    #[derive(Deserialize)]
    struct PrView {
        url: String,
        state: String,
    }

    let output = Command::new("gh")
        .args(["pr", "view", branch, "--json", "url,state"])
        .output()
        .context("Failed to run `gh pr view`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no pull requests found for branch") {
            return Ok(None);
        }
        return Err(anyhow!("`gh pr view` failed: {}", stderr.trim()));
    }

    let pr: PrView =
        serde_json::from_slice(&output.stdout).context("Failed to parse `gh pr view` output")?;

    if pr.state.eq_ignore_ascii_case("OPEN") {
        Ok(Some(OpenPrUrl { url: pr.url }))
    } else {
        Ok(None)
    }
}

/// Fetch editable details for an open PR on `branch`.
pub fn find_open_pr_for_edit(branch: &str) -> Result<Option<EditablePr>> {
    #[derive(Deserialize)]
    struct Label {
        name: String,
    }
    #[derive(Deserialize)]
    struct User {
        login: String,
    }
    #[derive(Deserialize)]
    struct ReviewRequest {
        #[serde(rename = "requestedReviewer")]
        requested_reviewer: Option<User>,
    }
    #[derive(Deserialize)]
    struct PrView {
        number: u64,
        title: String,
        body: String,
        url: String,
        state: String,
        labels: Vec<Label>,
        #[serde(rename = "reviewRequests")]
        review_requests: Vec<ReviewRequest>,
    }

    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            branch,
            "--json",
            "number,title,body,url,state,labels,reviewRequests",
        ])
        .output()
        .context("Failed to run `gh pr view`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no pull requests found for branch") {
            return Ok(None);
        }
        return Err(anyhow!("`gh pr view` failed: {}", stderr.trim()));
    }

    let pr: PrView =
        serde_json::from_slice(&output.stdout).context("Failed to parse `gh pr view` output")?;

    if !pr.state.eq_ignore_ascii_case("OPEN") {
        return Ok(None);
    }

    let labels = pr.labels.into_iter().map(|l| l.name).collect();
    let reviewers = pr
        .review_requests
        .into_iter()
        .filter_map(|r| r.requested_reviewer.map(|u| u.login))
        .collect();

    Ok(Some(EditablePr {
        number: pr.number,
        title: pr.title,
        body: pr.body,
        url: pr.url,
        labels,
        reviewers,
    }))
}

/// Fetch reviewer/check status details for a PR.
pub fn get_pr_status(owner: &str, repo: &str, pr_number: u64) -> Result<PrStatusSummary> {
    #[derive(Deserialize)]
    struct GraphQlResponse {
        data: GraphQlData,
    }
    #[derive(Deserialize)]
    struct GraphQlData {
        repository: Option<RepoData>,
    }
    #[derive(Deserialize)]
    struct RepoData {
        #[serde(rename = "pullRequest")]
        pull_request: Option<PullRequestData>,
    }
    #[derive(Deserialize)]
    struct PullRequestData {
        #[serde(rename = "reviewThreads")]
        review_threads: ReviewThreadConnection,
        #[serde(rename = "reviewRequests")]
        review_requests: ReviewRequestConnection,
        #[serde(rename = "latestReviews")]
        latest_reviews: LatestReviewConnection,
        commits: CommitConnection,
    }
    #[derive(Deserialize)]
    struct ReviewThreadConnection {
        nodes: Vec<ReviewThreadNode>,
    }
    #[derive(Deserialize)]
    struct ReviewThreadNode {
        #[serde(rename = "isResolved")]
        is_resolved: bool,
    }
    #[derive(Deserialize)]
    struct ReviewRequestConnection {
        nodes: Vec<ReviewRequestNode>,
    }
    #[derive(Deserialize)]
    struct ReviewRequestNode {
        #[serde(rename = "requestedReviewer")]
        requested_reviewer: Option<RequestedReviewer>,
    }
    #[derive(Deserialize)]
    struct RequestedReviewer {
        login: String,
    }
    #[derive(Deserialize)]
    struct LatestReviewConnection {
        nodes: Vec<LatestReviewNode>,
    }
    #[derive(Deserialize)]
    struct LatestReviewNode {
        state: String,
        author: Option<ReviewAuthor>,
    }
    #[derive(Deserialize)]
    struct ReviewAuthor {
        login: String,
    }
    #[derive(Deserialize)]
    struct CommitConnection {
        nodes: Vec<CommitNode>,
    }
    #[derive(Deserialize)]
    struct CommitNode {
        commit: CommitStatusNode,
    }
    #[derive(Deserialize)]
    struct CommitStatusNode {
        #[serde(rename = "statusCheckRollup")]
        status_check_rollup: Option<StatusCheckRollup>,
    }
    #[derive(Deserialize)]
    struct StatusCheckRollup {
        contexts: CheckContextConnection,
    }
    #[derive(Deserialize)]
    struct CheckContextConnection {
        nodes: Vec<CheckContextNode>,
    }
    #[derive(Deserialize)]
    #[serde(tag = "__typename")]
    enum CheckContextNode {
        CheckRun {
            name: String,
            status: Option<String>,
            conclusion: Option<String>,
        },
        StatusContext {
            context: String,
            state: String,
        },
    }

    let query = r#"
query($owner: String!, $repo: String!, $number: Int!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 100) {
        nodes {
          isResolved
        }
      }
      reviewRequests(first: 100) {
        nodes {
          requestedReviewer {
            ... on User {
              login
            }
          }
        }
      }
      latestReviews(first: 100) {
        nodes {
          state
          author {
            login
          }
        }
      }
      commits(last: 1) {
        nodes {
          commit {
            statusCheckRollup {
              contexts(first: 100) {
                nodes {
                  __typename
                  ... on CheckRun {
                    name
                    status
                    conclusion
                  }
                  ... on StatusContext {
                    context
                    state
                  }
                }
              }
            }
          }
        }
      }
    }
  }
}
"#;

    let output = Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={}", query)])
        .args(["-F", &format!("owner={}", owner)])
        .args(["-F", &format!("repo={}", repo)])
        .args(["-F", &format!("number={}", pr_number)])
        .output()
        .context("Failed to run `gh api graphql`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "Failed to fetch PR status details: {}",
            stderr.trim()
        ));
    }

    let parsed: GraphQlResponse =
        serde_json::from_slice(&output.stdout).context("Failed to parse graphql output")?;
    let pr = parsed
        .data
        .repository
        .and_then(|r| r.pull_request)
        .ok_or_else(|| anyhow!("PR not found in graphql response"))?;

    let unresolved_comments = pr
        .review_threads
        .nodes
        .into_iter()
        .filter(|thread| !thread.is_resolved)
        .count();

    let mut reviewer_map: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    for review in pr.latest_reviews.nodes {
        if let Some(author) = review.author {
            let status = match review.state.as_str() {
                "APPROVED" => "approved",
                "CHANGES_REQUESTED" => "requested changes",
                "COMMENTED" => "comments",
                _ => "comments",
            };
            reviewer_map.insert(author.login, status.to_string());
        }
    }
    for req in pr.review_requests.nodes {
        if let Some(reviewer) = req.requested_reviewer {
            reviewer_map.insert(reviewer.login, "waiting".to_string());
        }
    }

    let reviewer_statuses = reviewer_map
        .into_iter()
        .map(|(reviewer, status)| ReviewerStatus { reviewer, status })
        .collect();

    let mut running_checks_set = BTreeSet::new();
    let mut failed_checks_set = BTreeSet::new();

    if let Some(last_commit) = pr.commits.nodes.into_iter().last()
        && let Some(rollup) = last_commit.commit.status_check_rollup
    {
        for node in rollup.contexts.nodes {
            match node {
                CheckContextNode::CheckRun {
                    name,
                    status,
                    conclusion,
                } => {
                    let status_upper = status.unwrap_or_default().to_uppercase();
                    let conclusion_upper = conclusion.unwrap_or_default().to_uppercase();
                    if matches!(
                        status_upper.as_str(),
                        "IN_PROGRESS" | "PENDING" | "QUEUED" | "WAITING" | "REQUESTED"
                    ) {
                        running_checks_set.insert(name);
                    } else if matches!(
                        conclusion_upper.as_str(),
                        "FAILURE"
                            | "TIMED_OUT"
                            | "CANCELLED"
                            | "ACTION_REQUIRED"
                            | "STARTUP_FAILURE"
                            | "STALE"
                    ) {
                        failed_checks_set.insert(name);
                    }
                }
                CheckContextNode::StatusContext { context, state } => {
                    let state_upper = state.to_uppercase();
                    if state_upper == "PENDING" {
                        running_checks_set.insert(context);
                    } else if matches!(state_upper.as_str(), "ERROR" | "FAILURE") {
                        failed_checks_set.insert(context);
                    }
                }
            }
        }
    }

    Ok(PrStatusSummary {
        reviewer_statuses,
        unresolved_comments,
        running_checks: running_checks_set.into_iter().collect(),
        failed_checks: failed_checks_set.into_iter().collect(),
    })
}

/// Fetch review threads/comments for a PR.
pub fn get_pr_review_threads(
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> Result<Vec<PrReviewThread>> {
    #[derive(Deserialize)]
    struct GraphQlResponse {
        data: GraphQlData,
    }
    #[derive(Deserialize)]
    struct GraphQlData {
        repository: Option<RepoData>,
    }
    #[derive(Deserialize)]
    struct RepoData {
        #[serde(rename = "pullRequest")]
        pull_request: Option<PullRequestData>,
    }
    #[derive(Deserialize)]
    struct PullRequestData {
        #[serde(rename = "reviewThreads")]
        review_threads: ReviewThreadConnection,
    }
    #[derive(Deserialize)]
    struct ReviewThreadConnection {
        nodes: Vec<ReviewThreadNode>,
    }
    #[derive(Deserialize)]
    struct ReviewThreadNode {
        #[serde(rename = "isResolved")]
        is_resolved: bool,
        comments: ReviewCommentConnection,
    }
    #[derive(Deserialize)]
    struct ReviewCommentConnection {
        nodes: Vec<ReviewCommentNode>,
    }
    #[derive(Deserialize)]
    struct ReviewCommentNode {
        body: String,
        path: String,
        line: Option<u64>,
        #[serde(rename = "startLine")]
        start_line: Option<u64>,
        #[serde(rename = "originalLine")]
        original_line: Option<u64>,
        #[serde(rename = "originalStartLine")]
        original_start_line: Option<u64>,
        outdated: bool,
        #[serde(rename = "createdAt")]
        created_at: String,
        author: Option<ReviewCommentAuthorNode>,
    }
    #[derive(Deserialize)]
    struct ReviewCommentAuthorNode {
        login: String,
        #[serde(rename = "__typename")]
        type_name: String,
    }

    let query = r#"
query($owner: String!, $repo: String!, $number: Int!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 100) {
        nodes {
          isResolved
          comments(first: 100) {
            nodes {
              body
              path
              line
              startLine
              originalLine
              originalStartLine
              outdated
              createdAt
              author {
                __typename
                login
              }
            }
          }
        }
      }
    }
  }
}
"#;

    let output = Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={query}")])
        .args(["-F", &format!("owner={owner}")])
        .args(["-F", &format!("repo={repo}")])
        .args(["-F", &format!("number={pr_number}")])
        .output()
        .context("Failed to run `gh api graphql`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "Failed to fetch PR review comments: {}",
            stderr.trim()
        ));
    }

    let parsed: GraphQlResponse =
        serde_json::from_slice(&output.stdout).context("Failed to parse graphql output")?;
    let pr = parsed
        .data
        .repository
        .and_then(|r| r.pull_request)
        .ok_or_else(|| anyhow!("PR not found in graphql response"))?;

    let mut threads = Vec::new();
    for thread in pr.review_threads.nodes {
        let mut comments: Vec<PrReviewComment> = thread
            .comments
            .nodes
            .into_iter()
            .map(|comment| {
                let author = comment.author.map_or(
                    ReviewCommentAuthor {
                        login: "ghost".to_string(),
                        is_bot: false,
                    },
                    |author| ReviewCommentAuthor {
                        login: author.login,
                        is_bot: author.type_name == "Bot",
                    },
                );

                PrReviewComment {
                    author,
                    body: comment.body,
                    path: comment.path,
                    line: comment.line,
                    start_line: comment.start_line,
                    original_line: comment.original_line,
                    original_start_line: comment.original_start_line,
                    outdated: comment.outdated,
                    created_at: comment.created_at,
                }
            })
            .collect();

        comments.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        if comments.is_empty() {
            continue;
        }

        threads.push(PrReviewThread {
            is_resolved: thread.is_resolved,
            comments,
        });
    }

    threads.sort_by(|left, right| {
        left.comments[0]
            .created_at
            .cmp(&right.comments[0].created_at)
    });

    Ok(threads)
}

/// Update the base branch of an existing PR.
pub fn update_pr_base(pr_number: u64, new_base: &str) -> Result<()> {
    let status = Command::new("gh")
        .args(["pr", "edit", &pr_number.to_string(), "--base", new_base])
        .status()
        .context("Failed to run `gh pr edit`")?;

    if !status.success() {
        return Err(anyhow!("Failed to update base for PR #{}", pr_number));
    }
    Ok(())
}

/// Fetch the state of a PR (e.g., "OPEN", "MERGED", "CLOSED").
pub fn get_pr_state(pr_number: u64) -> Result<String> {
    #[derive(Deserialize)]
    struct PrView {
        state: String,
    }

    let output = Command::new("gh")
        .args(["pr", "view", &pr_number.to_string(), "--json", "state"])
        .output()
        .context("Failed to run `gh pr view`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("`gh pr view` failed: {}", stderr.trim()));
    }

    let pr: PrView =
        serde_json::from_slice(&output.stdout).context("Failed to parse `gh pr view` output")?;

    Ok(pr.state.to_uppercase())
}

/// Create a new PR. Returns the URL of the created PR.
pub fn create_pr(params: &CreatePrParams) -> Result<String> {
    let mut cmd = Command::new("gh");
    cmd.args([
        "pr",
        "create",
        "--title",
        &params.title,
        "--body",
        &params.body,
        "--base",
        &params.base,
        "--head",
        &params.head,
    ]);

    if params.draft {
        cmd.arg("--draft");
    }

    for label in &params.labels {
        cmd.args(["--label", label]);
    }

    for reviewer in &params.reviewers {
        cmd.args(["--reviewer", reviewer]);
    }

    let output = cmd.output().context("Failed to run `gh pr create`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("Failed to create PR: {}", stderr.trim()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub struct CreatePrParams {
    pub title: String,
    pub body: String,
    pub base: String,
    pub head: String,
    pub draft: bool,
    pub labels: Vec<String>,
    pub reviewers: Vec<String>,
}

/// Fetch all labels available in the repo. Returns a list of label names.
pub fn list_labels() -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Label {
        name: String,
    }

    let output = Command::new("gh")
        .args(["label", "list", "--json", "name", "--limit", "100"])
        .output()
        .context("Failed to run `gh label list`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("Failed to list labels: {}", stderr.trim()));
    }

    let labels: Vec<Label> =
        serde_json::from_slice(&output.stdout).context("Failed to parse `gh label list` output")?;

    Ok(labels.into_iter().map(|l| l.name).collect())
}

/// Fetch collaborators/assignable users for the current repo.
pub fn list_collaborators() -> Result<Vec<String>> {
    let output = Command::new("gh")
        .args([
            "api",
            "repos/{owner}/{repo}/collaborators",
            "--jq",
            ".[].login",
            "--paginate",
        ])
        .output()
        .context("Failed to run `gh api` for collaborators")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("Failed to list collaborators: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let logins: Vec<String> = stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    Ok(logins)
}

pub struct EditPrParams {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub current_labels: Vec<String>,
    pub labels: Vec<String>,
    pub current_reviewers: Vec<String>,
    pub reviewers: Vec<String>,
}

/// Edit an existing PR title/body/labels/reviewers.
pub fn edit_pr(params: &EditPrParams) -> Result<()> {
    let current_labels: BTreeSet<String> = params.current_labels.iter().cloned().collect();
    let labels: BTreeSet<String> = params.labels.iter().cloned().collect();
    let current_reviewers: BTreeSet<String> = params.current_reviewers.iter().cloned().collect();
    let reviewers: BTreeSet<String> = params.reviewers.iter().cloned().collect();

    let mut cmd = Command::new("gh");
    cmd.args([
        "pr",
        "edit",
        &params.number.to_string(),
        "--title",
        &params.title,
    ]);

    if let Some(body) = &params.body {
        cmd.args(["--body", body]);
    }

    for label in labels.difference(&current_labels) {
        cmd.args(["--add-label", label]);
    }
    for label in current_labels.difference(&labels) {
        cmd.args(["--remove-label", label]);
    }
    for reviewer in reviewers.difference(&current_reviewers) {
        cmd.args(["--add-reviewer", reviewer]);
    }
    for reviewer in current_reviewers.difference(&reviewers) {
        cmd.args(["--remove-reviewer", reviewer]);
    }

    let output = cmd.output().context("Failed to run `gh pr edit`")?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(anyhow!("Failed to edit PR #{}: {}", params.number, stderr))
    }
}

/// Open a URL in the default browser.
pub fn open_url(url: &str) -> Result<()> {
    if let Ok(command) = std::env::var("GITS_OPEN_COMMAND") {
        let status = Command::new(command)
            .arg(url)
            .status()
            .context("Failed to launch URL opener command from GITS_OPEN_COMMAND")?;
        if status.success() {
            return Ok(());
        }
        return Err(anyhow!(
            "URL opener command from GITS_OPEN_COMMAND failed with status {}",
            status
        ));
    }

    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };

    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };

    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };

    let status = cmd
        .status()
        .context("Failed to launch default browser opener")?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("Failed to open URL in browser: {}", url))
    }
}
