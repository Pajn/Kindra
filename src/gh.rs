use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};
use std::process::Command;

/// Verify that the `gh` CLI is installed and has credentials configured.
///
/// Reads the stored credential rather than running `gh auth status`, which
/// validates against the API and so costs a network round trip on every command
/// that touches GitHub. This still catches a missing CLI and a missing login;
/// a credential the server rejects surfaces from the first real call instead,
/// carrying `gh`'s own message.
pub fn check_gh() -> Result<()> {
    let status = Command::new("gh")
        .arg("auth")
        .arg("token")
        // The token must never reach this process's own output.
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
    pub head_ref_oid: Option<String>,
    pub review_decision: Option<String>,
    pub merge_state_status: String,
    pub mergeable: String,
    pub is_draft: bool,
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

/// A fully-detailed open PR as one snapshot query returns it. Carries
/// enough to serve both existence/base checks and editable-metadata needs, so the
/// whole `kin pr` flow can share one snapshot instead of querying per branch.
#[derive(Debug, Clone)]
pub struct OpenPr {
    pub number: u64,
    pub base_branch: String,
    /// Whether the PR's head branch lives in a fork rather than this repository.
    pub is_cross_repository: bool,
    pub is_draft: bool,
    pub author_login: Option<String>,
    pub title: String,
    pub body: String,
    pub url: String,
    pub labels: Vec<String>,
    pub reviewers: Vec<String>,
}

impl OpenPr {
    /// Project the editable subset of this PR's metadata.
    pub fn to_editable(&self) -> EditablePr {
        EditablePr {
            number: self.number,
            title: self.title.clone(),
            body: self.body.clone(),
            url: self.url.clone(),
            labels: self.labels.clone(),
            reviewers: self.reviewers.clone(),
        }
    }
}

/// The repository gh's PR commands act on, resolved the way `gh pr list` and
/// `gh pr create` resolve it (`GH_REPO`, the default chosen with
/// `gh repo set-default`, or the remotes), so a query pinned to it sees the same
/// pull requests. Resolve it once per command and pass it to every query.
#[derive(Debug, Clone)]
pub struct PrRepository {
    url: String,
    host: String,
    owner: String,
    name: String,
}

impl PrRepository {
    /// Ask `gh repo view` which repository gh's PR commands would use here.
    pub fn resolve() -> Result<Self> {
        #[derive(Deserialize)]
        struct RepoView {
            url: String,
        }
        let output = Command::new("gh")
            .args(["repo", "view", "--json", "url"])
            .output()
            .context("Failed to identify the PR repository")?;
        if !output.status.success() {
            return Err(anyhow!(
                "Could not identify PR repository: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let view: RepoView =
            serde_json::from_slice(&output.stdout).context("Invalid PR repository identity")?;
        let (host, owner, name) = repository_parts(&view.url)
            .ok_or_else(|| anyhow!("Could not identify PR repository from its URL"))?;
        Ok(Self {
            url: view.url,
            host,
            owner,
            name,
        })
    }

    /// Canonical host/owner/repository, as [`repository_identity`] renders it.
    pub fn identity(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.name).to_ascii_lowercase()
    }
}

/// Resolve gh's selected repository once and pin the PR query to that identity.
/// Remote selection must use this same repository, not an assumed origin.
pub fn checkout_pr_snapshot() -> Result<(String, HashMap<String, OpenPr>)> {
    let repository = PrRepository::resolve()?;
    Ok((repository.identity(), list_open_prs(&repository)?))
}

/// Canonical host/owner/repository for HTTPS, SSH URL and scp-style Git URLs.
/// Unrecognized URLs fail closed rather than guessing which repository they name.
pub(crate) fn repository_identity(url: &str) -> Option<String> {
    let (host, owner, name) = repository_parts(url)?;
    Some(format!("{host}/{owner}/{name}").to_ascii_lowercase())
}

/// Host, owner and repository name of a repository URL, as written.
fn repository_parts(url: &str) -> Option<(String, String, String)> {
    let (host, path) = if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("ssh://"))
    {
        let (authority, path) = rest.split_once('/')?;
        (authority.rsplit('@').next()?, path)
    } else {
        let (authority, path) = url.split_once(':')?;
        (authority.rsplit('@').next()?, path)
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let parts: Vec<_> = path.split('/').collect();
    if host.is_empty()
        || parts.len() != 2
        || parts
            .iter()
            .any(|part| part.is_empty() || *part == "." || *part == "..")
        || url
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '?' | '#' | '%' | '\\'))
    {
        return None;
    }
    Some((host.to_string(), parts[0].to_string(), parts[1].to_string()))
}

#[derive(Deserialize)]
struct PrAuthor {
    login: String,
}

/// A requested reviewer can be a user (has `login`) or a team (no `login`).
/// Keep `login` optional so a team reviewer does not fail the whole parse.
#[derive(Deserialize)]
struct PrReviewer {
    #[serde(default)]
    login: Option<String>,
}

/// GraphQL nests the reviewer under `requestedReviewer`; `gh pr list --json`
/// reports it flattened, with `login` beside `__typename`.
#[derive(Deserialize)]
struct PrReviewRequest {
    #[serde(rename = "requestedReviewer", default)]
    requested_reviewer: Option<PrReviewer>,
    #[serde(default)]
    login: Option<String>,
}

impl PrReviewRequest {
    fn login(self) -> Option<String> {
        self.requested_reviewer
            .and_then(|reviewer| reviewer.login)
            .or(self.login)
    }
}

#[derive(Deserialize)]
struct PrLabel {
    name: String,
}

/// Add `pr` under its head branch name. Two open PRs can share a head branch
/// name when one of them comes from a fork, and callers look branches up by
/// name alone. A stack branch pushes to this repository, so prefer the PR
/// whose head is here; let a fork's PR claim the name only when nothing in
/// this repository does. Among equals the first one added wins, so add them
/// newest first, as `gh pr list` returns them. Without this the last one
/// parsed would win, and `pr edit` or `pr merge` could act on a contributor's
/// PR.
fn insert_open_pr(map: &mut HashMap<String, OpenPr>, head_ref_name: String, pr: OpenPr) {
    match map.entry(head_ref_name) {
        std::collections::hash_map::Entry::Occupied(mut existing) => {
            if existing.get().is_cross_repository && !pr.is_cross_repository {
                existing.insert(pr);
            }
        }
        std::collections::hash_map::Entry::Vacant(slot) => {
            slot.insert(pr);
        }
    }
}

/// Fetch every open PR in `repository` in a single `gh pr list` call, keyed by
/// head branch name. In a repository with hundreds of open PRs this is slow, so
/// use it only where any PR may matter; when the branches are known, use
/// [`open_prs_for_branches`].
pub fn list_open_prs(repository: &PrRepository) -> Result<HashMap<String, OpenPr>> {
    #[derive(Deserialize)]
    struct PrListItem {
        number: u64,
        #[serde(rename = "headRefName", default)]
        head_ref_name: String,
        #[serde(rename = "isCrossRepository", default)]
        is_cross_repository: bool,
        #[serde(rename = "baseRefName", default)]
        base_ref_name: String,
        #[serde(rename = "isDraft", default)]
        is_draft: bool,
        #[serde(default)]
        author: Option<PrAuthor>,
        #[serde(default)]
        title: String,
        #[serde(default)]
        body: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        labels: Vec<PrLabel>,
        #[serde(rename = "reviewRequests", default)]
        review_requests: Vec<PrReviewRequest>,
    }

    let output = Command::new("gh")
        .args([
            "pr",
            "list",
            "--state",
            "open",
            "--limit",
            "500",
            "--json",
            "number,headRefName,isCrossRepository,baseRefName,isDraft,author,title,body,url,labels,reviewRequests",
            "--repo",
            &repository.url,
        ])
        .output()
        .context("Failed to run `gh pr list`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("`gh pr list` failed: {}", stderr.trim()));
    }

    let items: Vec<PrListItem> =
        serde_json::from_slice(&output.stdout).context("Failed to parse `gh pr list` output")?;

    let mut map: HashMap<String, OpenPr> = HashMap::with_capacity(items.len());
    for item in items {
        if item.head_ref_name.is_empty() {
            continue;
        }
        let pr = OpenPr {
            number: item.number,
            base_branch: item.base_ref_name,
            is_cross_repository: item.is_cross_repository,
            is_draft: item.is_draft,
            author_login: item.author.map(|author| author.login),
            title: item.title,
            body: item.body,
            url: item.url,
            labels: item.labels.into_iter().map(|l| l.name).collect(),
            reviewers: item
                .review_requests
                .into_iter()
                .filter_map(PrReviewRequest::login)
                .collect(),
        };
        insert_open_pr(&mut map, item.head_ref_name, pr);
    }

    Ok(map)
}

/// How many head branches one [`open_prs_for_branches`] request asks about.
/// Bounds the query text and GitHub's per-query node budget on a large stack.
pub const OPEN_PRS_HEAD_BATCH: usize = 50;

/// How many open PRs to fetch per head branch in one page. Several open PRs
/// share a head name only when forks use it too (or one branch targets several
/// bases), so one page almost always holds them all. When it does not and no
/// PR from this repository is among them yet, that branch is paged further.
const OPEN_PRS_PER_HEAD: usize = 10;

/// Fields of one open PR, selected the way `gh pr list --json` reports them.
const OPEN_PR_FIELDS: &str = "pageInfo { hasNextPage endCursor } nodes { number headRefName \
isCrossRepository baseRefName isDraft author { login } title body url \
labels(first: 100) { nodes { name } } \
reviewRequests(first: 100) { nodes { requestedReviewer { ... on User { login } } } } }";

#[derive(Default, Deserialize)]
struct OpenPrPageInfo {
    #[serde(rename = "hasNextPage", default)]
    has_next_page: bool,
    #[serde(rename = "endCursor", default)]
    end_cursor: Option<String>,
}
#[derive(Deserialize)]
struct Nodes<T> {
    #[serde(default = "Vec::new")]
    nodes: Vec<T>,
}
#[derive(Deserialize)]
struct PrNode {
    number: u64,
    #[serde(rename = "headRefName", default)]
    head_ref_name: String,
    #[serde(rename = "isCrossRepository", default)]
    is_cross_repository: bool,
    #[serde(rename = "baseRefName", default)]
    base_ref_name: String,
    #[serde(rename = "isDraft", default)]
    is_draft: bool,
    #[serde(default)]
    author: Option<PrAuthor>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    labels: Option<Nodes<PrLabel>>,
    #[serde(rename = "reviewRequests", default)]
    review_requests: Option<Nodes<PrReviewRequest>>,
}
/// One page of the open PRs with a given head branch name.
#[derive(Deserialize)]
struct PrConnection {
    #[serde(rename = "pageInfo", default)]
    page_info: OpenPrPageInfo,
    #[serde(default)]
    nodes: Vec<PrNode>,
}

/// Add the PRs of `nodes` whose head is exactly `head` to `map`.
fn add_open_pr_nodes(map: &mut HashMap<String, OpenPr>, head: &str, nodes: Vec<PrNode>) {
    for node in nodes {
        // Match exactly, as a lookup by branch name in `gh pr list` does.
        if node.head_ref_name != head {
            continue;
        }
        let pr = OpenPr {
            number: node.number,
            base_branch: node.base_ref_name,
            is_cross_repository: node.is_cross_repository,
            is_draft: node.is_draft,
            author_login: node.author.map(|author| author.login),
            title: node.title,
            body: node.body,
            url: node.url,
            labels: node
                .labels
                .map(|labels| labels.nodes.into_iter().map(|l| l.name).collect())
                .unwrap_or_default(),
            reviewers: node
                .review_requests
                .map(|requests| {
                    requests
                        .nodes
                        .into_iter()
                        .filter_map(PrReviewRequest::login)
                        .collect()
                })
                .unwrap_or_default(),
        };
        insert_open_pr(map, node.head_ref_name, pr);
    }
}

/// Fetch the open PRs whose head is one of `branches` in `repository`, keyed by
/// head branch name, with the same semantics as [`list_open_prs`]: a PR from
/// this repository wins over a fork's PR with the same head name, and a team
/// reviewer (no login) is left out.
///
/// Asks only about the named branches, one alias per branch in a single
/// `gh api graphql` request per [`OPEN_PRS_HEAD_BATCH`] branches, instead of
/// listing every open PR in the repository, which in a busy repository returns
/// hundreds of PRs and takes seconds. An empty set of branches asks nothing.
pub fn open_prs_for_branches<S: AsRef<str>>(
    repository: &PrRepository,
    branches: &[S],
) -> Result<HashMap<String, OpenPr>> {
    #[derive(Deserialize)]
    struct HeadsData {
        /// Aliases are generated per branch, so the selection comes back as a
        /// map rather than a fixed set of fields.
        repository: Option<HashMap<String, Option<PrConnection>>>,
    }

    let heads: BTreeSet<&str> = branches
        .iter()
        .map(AsRef::as_ref)
        .filter(|name| !name.is_empty())
        .collect();
    let heads: Vec<&str> = heads.into_iter().collect();

    // Newest first, as `gh pr list` orders them, so the same PR wins a tie.
    let connection = |head: &str, first: usize, after: &str| {
        format!(
            "pullRequests(headRefName: ${head}, states: OPEN, first: {first}{after}, \
             orderBy: {{field: CREATED_AT, direction: DESC}}) {{ {OPEN_PR_FIELDS} }}"
        )
    };
    let fetch = |declarations: &str, selections: &str, variables: &[(&str, &str)]| {
        let query = format!(
            "query($owner: String!, $name: String!{declarations}) {{ \
             repository(owner: $owner, name: $name) {{ {selections} }} }}"
        );
        let mut fields = vec![
            ("owner", repository.owner.as_str()),
            ("name", repository.name.as_str()),
        ];
        fields.extend_from_slice(variables);
        let data: HeadsData =
            run_graphql(&repository.host, &query, &fields).context("Failed to fetch open PRs")?;
        data.repository
            .ok_or_else(|| anyhow!("Repository not found in graphql response"))
    };

    let mut map = HashMap::new();
    let settled = |map: &HashMap<String, OpenPr>, head: &str| {
        map.get(head).is_some_and(|pr| !pr.is_cross_repository)
    };

    for chunk in heads.chunks(OPEN_PRS_HEAD_BATCH) {
        let mut declarations = String::new();
        let mut selections = String::new();
        let mut variables = Vec::with_capacity(chunk.len());
        let aliases: Vec<String> = (0..chunk.len()).map(|index| format!("h{index}")).collect();
        for (alias, head) in aliases.iter().zip(chunk) {
            // Branch names travel as variables, never inside the query text.
            declarations.push_str(&format!(", ${alias}: String!"));
            selections.push_str(&format!(
                "{alias}: {} ",
                connection(alias, OPEN_PRS_PER_HEAD, "")
            ));
            variables.push((alias.as_str(), *head));
        }
        let mut aliased = fetch(&declarations, &selections, &variables)?;

        for (alias, head) in aliases.iter().zip(chunk) {
            let Some(mut page) = aliased.remove(alias).flatten() else {
                continue;
            };
            let mut previous_cursor: Option<String> = None;
            loop {
                let next = page.page_info;
                add_open_pr_nodes(&mut map, head, page.nodes);
                // Later pages hold only older PRs, which cannot displace a PR
                // from this repository already found.
                if !next.has_next_page || settled(&map, head) {
                    break;
                }
                let cursor = next
                    .end_cursor
                    .ok_or_else(|| anyhow!("Missing cursor while paginating open PRs"))?;
                if previous_cursor.as_deref() == Some(cursor.as_str()) {
                    return Err(anyhow!(
                        "Open PR pagination for '{head}' did not advance past cursor {cursor}"
                    ));
                }
                let mut more = fetch(
                    ", $h0: String!, $after: String!",
                    &format!("h0: {}", connection("h0", 100, ", after: $after")),
                    &[("h0", head), ("after", &cursor)],
                )?;
                previous_cursor = Some(cursor);
                match more.remove("h0").flatten() {
                    Some(following) => page = following,
                    None => break,
                }
            }
        }
    }

    Ok(map)
}

/// Run a `gh api graphql` query against `host` and return its `data`. Every
/// variable is a string, so each is passed raw (`-f`): a branch name that looks
/// like a number or `true` must not be coerced into another JSON type. Fails on
/// a non-zero exit and on any GraphQL `errors` in the response.
fn run_graphql<T: serde::de::DeserializeOwned>(
    host: &str,
    query: &str,
    variables: &[(&str, &str)],
) -> Result<T> {
    #[derive(Deserialize)]
    struct GraphQlError {
        message: String,
    }
    #[derive(Deserialize)]
    struct Response<T> {
        data: Option<T>,
        #[serde(default)]
        errors: Vec<GraphQlError>,
    }

    let mut command = Command::new("gh");
    command.args(["api", "graphql", "--hostname", host]);
    command.args(["-f", &format!("query={query}")]);
    for (key, value) in variables {
        command.args(["-f", &format!("{key}={value}")]);
    }
    let output = command.output().context("Failed to run `gh api graphql`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("`gh api graphql` failed: {}", stderr.trim()));
    }
    let response: Response<T> =
        serde_json::from_slice(&output.stdout).context("Failed to parse graphql output")?;
    if !response.errors.is_empty() {
        let messages: Vec<_> = response.errors.into_iter().map(|e| e.message).collect();
        return Err(anyhow!("GraphQL error: {}", messages.join("; ")));
    }
    response
        .data
        .ok_or_else(|| anyhow!("graphql response carried no data"))
}

pub fn current_user_login() -> Result<String> {
    let output = Command::new("gh")
        .args(["api", "user", "--jq", ".login"])
        .output()
        .context("Failed to run `gh api user`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "Failed to fetch current GitHub user: {}",
            stderr.trim()
        ));
    }

    let login = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if login.is_empty() {
        return Err(anyhow!("GitHub CLI returned an empty current user login."));
    }

    Ok(login)
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

/// Fetch reviewer/check status details for a PR.
/// How many PRs to request per batched status query. Bounds both the generated
/// query text and GitHub's per-query complexity budget on a large stack.
const PR_STATUS_BATCH: usize = 20;

/// Fetch reviewer/check status details for a PR.
pub fn get_pr_status(owner: &str, repo: &str, pr_number: u64) -> Result<PrStatusSummary> {
    get_pr_statuses(owner, repo, &[pr_number])?
        .remove(&pr_number)
        .ok_or_else(|| anyhow!("PR not found in graphql response"))
}

/// Fetch reviewer/check status details for several PRs, keyed by PR number.
///
/// One GraphQL query can select many pull requests under separate aliases, so a
/// whole stack costs a single `gh` invocation rather than one per PR. Only a
/// connection that overflows its first page needs further requests, which is
/// rare, and those are still paged per PR.
pub fn get_pr_statuses(
    owner: &str,
    repo: &str,
    pr_numbers: &[u64],
) -> Result<HashMap<u64, PrStatusSummary>> {
    #[derive(Deserialize)]
    struct PullRequestData {
        #[serde(rename = "reviewThreads")]
        review_threads: ReviewThreadConnection,
        #[serde(rename = "reviewRequests")]
        review_requests: ReviewRequestConnection,
        #[serde(rename = "latestReviews")]
        latest_reviews: LatestReviewConnection,
        #[serde(rename = "headRefOid", default)]
        head_ref_oid: Option<String>,
        #[serde(rename = "reviewDecision", default)]
        review_decision: Option<String>,
        #[serde(rename = "mergeStateStatus", default)]
        merge_state_status: String,
        #[serde(default)]
        mergeable: String,
        #[serde(rename = "isDraft", default)]
        is_draft: bool,
        commits: CommitConnection,
    }
    #[derive(Default, Deserialize)]
    struct PageInfo {
        #[serde(rename = "hasNextPage")]
        has_next_page: bool,
        #[serde(rename = "endCursor")]
        end_cursor: Option<String>,
    }
    #[derive(Deserialize)]
    struct ReviewThreadConnection {
        #[serde(rename = "pageInfo", default)]
        page_info: PageInfo,
        nodes: Vec<ReviewThreadNode>,
    }
    #[derive(Deserialize)]
    struct ReviewThreadNode {
        #[serde(rename = "isResolved")]
        is_resolved: bool,
    }
    #[derive(Deserialize)]
    struct ReviewRequestConnection {
        #[serde(rename = "pageInfo", default)]
        page_info: PageInfo,
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
        #[serde(rename = "pageInfo", default)]
        page_info: PageInfo,
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
        oid: Option<String>,
        #[serde(rename = "statusCheckRollup")]
        status_check_rollup: Option<StatusCheckRollup>,
    }
    #[derive(Deserialize)]
    struct StatusCheckRollup {
        contexts: CheckContextConnection,
    }
    #[derive(Deserialize)]
    struct CheckContextConnection {
        #[serde(rename = "pageInfo", default)]
        page_info: PageInfo,
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

    // Envelopes for the per-connection follow-up pagination queries.
    #[derive(Deserialize)]
    struct ThreadsPage {
        data: ThreadsPageData,
    }
    #[derive(Deserialize)]
    struct ThreadsPageData {
        repository: Option<ThreadsPageRepo>,
    }
    #[derive(Deserialize)]
    struct ThreadsPageRepo {
        #[serde(rename = "pullRequest")]
        pull_request: Option<ThreadsPagePr>,
    }
    #[derive(Deserialize)]
    struct ThreadsPagePr {
        #[serde(rename = "reviewThreads")]
        review_threads: ReviewThreadConnection,
    }
    #[derive(Deserialize)]
    struct RequestsPage {
        data: RequestsPageData,
    }
    #[derive(Deserialize)]
    struct RequestsPageData {
        repository: Option<RequestsPageRepo>,
    }
    #[derive(Deserialize)]
    struct RequestsPageRepo {
        #[serde(rename = "pullRequest")]
        pull_request: Option<RequestsPagePr>,
    }
    #[derive(Deserialize)]
    struct RequestsPagePr {
        #[serde(rename = "reviewRequests")]
        review_requests: ReviewRequestConnection,
    }
    #[derive(Deserialize)]
    struct ReviewsPage {
        data: ReviewsPageData,
    }
    #[derive(Deserialize)]
    struct ReviewsPageData {
        repository: Option<ReviewsPageRepo>,
    }
    #[derive(Deserialize)]
    struct ReviewsPageRepo {
        #[serde(rename = "pullRequest")]
        pull_request: Option<ReviewsPagePr>,
    }
    #[derive(Deserialize)]
    struct ReviewsPagePr {
        #[serde(rename = "latestReviews")]
        latest_reviews: LatestReviewConnection,
    }
    #[derive(Deserialize)]
    struct ContextsPage {
        data: ContextsPageData,
    }
    #[derive(Deserialize)]
    struct ContextsPageData {
        repository: Option<ContextsPageRepo>,
    }
    #[derive(Deserialize)]
    struct ContextsPageRepo {
        object: Option<ContextsPageCommit>,
    }
    #[derive(Deserialize)]
    struct ContextsPageCommit {
        #[serde(rename = "statusCheckRollup")]
        status_check_rollup: Option<StatusCheckRollup>,
    }

    #[derive(Deserialize)]
    struct BatchResponse {
        data: BatchData,
    }
    #[derive(Deserialize)]
    struct BatchData {
        /// Aliases are generated per PR, so the selection comes back as a map
        /// rather than a fixed set of fields.
        repository: Option<HashMap<String, Option<PullRequestData>>>,
    }

    // The selection set for one pull request, repeated under an alias per PR.
    let pr_fields = r#"
      reviewThreads(first: 100) {
        pageInfo {
          hasNextPage
          endCursor
        }
        nodes {
          isResolved
        }
      }
      reviewRequests(first: 100) {
        pageInfo {
          hasNextPage
          endCursor
        }
        nodes {
          requestedReviewer {
            ... on User {
              login
            }
          }
        }
      }
      latestReviews(first: 100) {
        pageInfo {
          hasNextPage
          endCursor
        }
        nodes {
          state
          author {
            login
          }
        }
      }
      headRefOid
      reviewDecision
      mergeStateStatus
      mergeable
      isDraft
      commits(last: 1) {
        nodes {
          commit {
            oid
            statusCheckRollup {
              contexts(first: 100) {
                pageInfo {
                  hasNextPage
                  endCursor
                }
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
"#;
    // Follow-up queries page through each connection independently once the
    // initial page reports hasNextPage.
    let threads_query = r#"
query($owner: String!, $repo: String!, $number: Int!, $cursor: String!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $cursor) {
        pageInfo {
          hasNextPage
          endCursor
        }
        nodes {
          isResolved
        }
      }
    }
  }
}
"#;
    let requests_query = r#"
query($owner: String!, $repo: String!, $number: Int!, $cursor: String!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewRequests(first: 100, after: $cursor) {
        pageInfo {
          hasNextPage
          endCursor
        }
        nodes {
          requestedReviewer {
            ... on User {
              login
            }
          }
        }
      }
    }
  }
}
"#;
    let reviews_query = r#"
query($owner: String!, $repo: String!, $number: Int!, $cursor: String!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      latestReviews(first: 100, after: $cursor) {
        pageInfo {
          hasNextPage
          endCursor
        }
        nodes {
          state
          author {
            login
          }
        }
      }
    }
  }
}
"#;
    let contexts_query = r#"
query($owner: String!, $repo: String!, $oid: GitObjectID!, $cursor: String!) {
  repository(owner: $owner, name: $repo) {
    object(oid: $oid) {
      ... on Commit {
        statusCheckRollup {
          contexts(first: 100, after: $cursor) {
            pageInfo {
              hasNextPage
              endCursor
            }
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
"#;

    // Runs a graphql query with the given fields, returning the raw stdout bytes
    // for the caller to deserialize. The only Int variable is `number`, which must
    // be passed typed (`-F`); every other variable (owner/repo/cursor are String,
    // oid is GitObjectID) is passed raw (`-f`) so a numeric-looking cursor or oid
    // can't be coerced to a non-string JSON type and rejected by the API.
    let run_query = |query: &str, fields: &[(&str, &str)]| -> Result<Vec<u8>> {
        let mut command = Command::new("gh");
        command.args(["api", "graphql", "-f", &format!("query={query}")]);
        for (key, value) in fields {
            let flag = if *key == "number" { "-F" } else { "-f" };
            command.args([flag, &format!("{key}={value}")]);
        }
        let output = command.output().context("Failed to run `gh api graphql`")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "Failed to fetch PR status details: {}",
                stderr.trim()
            ));
        }
        Ok(output.stdout)
    };

    // Follow a GraphQL connection's `pageInfo` cursors, starting from an
    // already-fetched first page, until exhausted. `fetch` runs one more page for
    // the given end-cursor and returns its (nodes, next page_info). Centralizes
    // the cursor / has_next_page bookkeeping every paginated connection shares,
    // so a fix to it can't miss one of them.
    fn collect_all_pages<T>(
        first_nodes: Vec<T>,
        first_page: PageInfo,
        mut fetch: impl FnMut(&str) -> Result<(Vec<T>, PageInfo)>,
    ) -> Result<Vec<T>> {
        let mut nodes = first_nodes;
        let mut page = first_page;
        while page.has_next_page {
            let cursor = page
                .end_cursor
                .ok_or_else(|| anyhow!("Missing cursor while paginating GraphQL results"))?;
            let (next_nodes, next_page) = fetch(&cursor)?;
            nodes.extend(next_nodes);
            page = next_page;
        }
        Ok(nodes)
    }

    // Reduce one PR's first page to a summary, following any connection that
    // reports further pages.
    let summarize = |number: u64, pr: PullRequestData| -> Result<PrStatusSummary> {
        let number = number.to_string();

        // Collect the first page of each connection, then follow cursors until
        // exhausted so the merge-readiness gate sees every thread/review/check.
        let thread_nodes = collect_all_pages(
            pr.review_threads.nodes,
            pr.review_threads.page_info,
            |cursor| {
                let stdout = run_query(
                    threads_query,
                    &[
                        ("owner", owner),
                        ("repo", repo),
                        ("number", &number),
                        ("cursor", cursor),
                    ],
                )?;
                let page: ThreadsPage =
                    serde_json::from_slice(&stdout).context("Failed to parse graphql output")?;
                let connection = page
                    .data
                    .repository
                    .and_then(|r| r.pull_request)
                    .map(|pr| pr.review_threads)
                    .ok_or_else(|| anyhow!("PR not found in graphql response"))?;
                Ok((connection.nodes, connection.page_info))
            },
        )?;

        let request_nodes = collect_all_pages(
            pr.review_requests.nodes,
            pr.review_requests.page_info,
            |cursor| {
                let stdout = run_query(
                    requests_query,
                    &[
                        ("owner", owner),
                        ("repo", repo),
                        ("number", &number),
                        ("cursor", cursor),
                    ],
                )?;
                let page: RequestsPage =
                    serde_json::from_slice(&stdout).context("Failed to parse graphql output")?;
                let connection = page
                    .data
                    .repository
                    .and_then(|r| r.pull_request)
                    .map(|pr| pr.review_requests)
                    .ok_or_else(|| anyhow!("PR not found in graphql response"))?;
                Ok((connection.nodes, connection.page_info))
            },
        )?;

        let review_nodes = collect_all_pages(
            pr.latest_reviews.nodes,
            pr.latest_reviews.page_info,
            |cursor| {
                let stdout = run_query(
                    reviews_query,
                    &[
                        ("owner", owner),
                        ("repo", repo),
                        ("number", &number),
                        ("cursor", cursor),
                    ],
                )?;
                let page: ReviewsPage =
                    serde_json::from_slice(&stdout).context("Failed to parse graphql output")?;
                let connection = page
                    .data
                    .repository
                    .and_then(|r| r.pull_request)
                    .map(|pr| pr.latest_reviews)
                    .ok_or_else(|| anyhow!("PR not found in graphql response"))?;
                Ok((connection.nodes, connection.page_info))
            },
        )?;

        let unresolved_comments = thread_nodes
            .into_iter()
            .filter(|thread| !thread.is_resolved)
            .count();

        let mut reviewer_map: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for review in review_nodes {
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
        for req in request_nodes {
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

        let mut context_nodes = Vec::new();
        if let Some(last_commit) = pr.commits.nodes.into_iter().last() {
            let commit_oid = last_commit.commit.oid;
            if let Some(rollup) = last_commit.commit.status_check_rollup {
                context_nodes = collect_all_pages(
                    rollup.contexts.nodes,
                    rollup.contexts.page_info,
                    |cursor| {
                        let oid = commit_oid
                            .as_deref()
                            .ok_or_else(|| anyhow!("Missing commit oid while paginating checks"))?;
                        let stdout = run_query(
                            contexts_query,
                            &[
                                ("owner", owner),
                                ("repo", repo),
                                ("oid", oid),
                                ("cursor", cursor),
                            ],
                        )?;
                        let page: ContextsPage = serde_json::from_slice(&stdout)
                            .context("Failed to parse graphql output")?;
                        let connection = page
                            .data
                            .repository
                            .and_then(|r| r.object)
                            .and_then(|commit| commit.status_check_rollup)
                            .map(|rollup| rollup.contexts)
                            .ok_or_else(|| anyhow!("Commit not found in graphql response"))?;
                        Ok((connection.nodes, connection.page_info))
                    },
                )?;
            }
        }

        for node in context_nodes {
            match node {
                CheckContextNode::CheckRun {
                    name,
                    status,
                    conclusion,
                } => {
                    let status_upper = status.unwrap_or_default().to_uppercase();
                    let conclusion_upper = conclusion.unwrap_or_default().to_uppercase();
                    // COMPLETED is the only terminal CheckRun status. Fail closed:
                    // a completed run is green only for an explicitly-passing
                    // conclusion; any other (including an unknown/future or empty
                    // conclusion) counts as failed, and any non-terminal or
                    // unrecognized status counts as still running. That way an
                    // unrecognized check state can never read as mergeable.
                    if status_upper == "COMPLETED" {
                        if !matches!(conclusion_upper.as_str(), "SUCCESS" | "NEUTRAL" | "SKIPPED") {
                            failed_checks_set.insert(name);
                        }
                    } else {
                        running_checks_set.insert(name);
                    }
                }
                CheckContextNode::StatusContext { context, state } => {
                    // Fail closed: only an explicit SUCCESS is green; ERROR/FAILURE
                    // fail; PENDING, EXPECTED (a required status not yet reported),
                    // and any unknown/empty state block as still running rather than
                    // being silently treated as passing.
                    let state_upper = state.to_uppercase();
                    match state_upper.as_str() {
                        "SUCCESS" => {}
                        "ERROR" | "FAILURE" => {
                            failed_checks_set.insert(context);
                        }
                        _ => {
                            running_checks_set.insert(context);
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
            head_ref_oid: pr.head_ref_oid,
            review_decision: pr.review_decision,
            merge_state_status: pr.merge_state_status,
            mergeable: pr.mergeable,
            is_draft: pr.is_draft,
        })
    };

    let mut summaries = HashMap::new();
    for chunk in pr_numbers.chunks(PR_STATUS_BATCH) {
        let mut selections = String::new();
        for (index, number) in chunk.iter().enumerate() {
            // `number` is a u64, so it can only ever render as digits.
            selections.push_str(&format!(
                "    pr{index}: pullRequest(number: {number}) {{{pr_fields}}}\n"
            ));
        }
        let query = format!(
            "query($owner: String!, $repo: String!) {{\n  repository(owner: $owner, name: $repo) {{\n{selections}  }}\n}}\n"
        );

        let stdout = run_query(&query, &[("owner", owner), ("repo", repo)])?;
        let parsed: BatchResponse =
            serde_json::from_slice(&stdout).context("Failed to parse graphql output")?;
        let mut aliased = parsed
            .data
            .repository
            .ok_or_else(|| anyhow!("Repository not found in graphql response"))?;

        for (index, number) in chunk.iter().enumerate() {
            let pr = aliased
                .remove(&format!("pr{index}"))
                .flatten()
                .ok_or_else(|| anyhow!("PR #{number} not found in graphql response"))?;
            summaries.insert(*number, summarize(*number, pr)?);
        }
    }

    Ok(summaries)
}

/// Fetch review threads/comments for a PR.
pub fn get_pr_review_threads(
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> Result<Vec<PrReviewThread>> {
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
        #[serde(rename = "pageInfo", default)]
        page_info: PageInfo,
        nodes: Vec<ReviewThreadNode>,
    }
    #[derive(Deserialize)]
    struct ReviewThreadNode {
        id: Option<String>,
        #[serde(rename = "isResolved")]
        is_resolved: bool,
        comments: ReviewCommentConnection,
    }
    #[derive(Deserialize)]
    struct ReviewCommentConnection {
        #[serde(rename = "pageInfo", default)]
        page_info: PageInfo,
        nodes: Vec<ReviewCommentNode>,
    }
    #[derive(Default, Deserialize)]
    struct PageInfo {
        #[serde(rename = "hasNextPage")]
        has_next_page: bool,
        #[serde(rename = "endCursor")]
        end_cursor: Option<String>,
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
    #[derive(Deserialize)]
    struct ReviewThreadPageResponse {
        data: GraphQlData,
    }
    #[derive(Deserialize)]
    struct ReviewThreadCommentsPageResponse {
        data: ReviewThreadCommentsPageData,
    }
    #[derive(Deserialize)]
    struct ReviewThreadCommentsPageData {
        node: Option<ReviewThreadCommentsNode>,
    }
    #[derive(Deserialize)]
    struct ReviewThreadCommentsNode {
        comments: ReviewCommentConnection,
    }

    let query = r#"
query($owner: String!, $repo: String!, $number: Int!, $threadCursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $threadCursor) {
        pageInfo {
          hasNextPage
          endCursor
        }
        nodes {
          id
          isResolved
          comments(first: 100) {
            pageInfo {
              hasNextPage
              endCursor
            }
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
    let comments_query = r#"
query($threadId: ID!, $commentsCursor: String) {
  node(id: $threadId) {
    ... on PullRequestReviewThread {
      comments(first: 100, after: $commentsCursor) {
        pageInfo {
          hasNextPage
          endCursor
        }
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
"#;

    let mut review_threads = Vec::new();
    let mut thread_cursor = None;

    loop {
        let mut command = Command::new("gh");
        command
            .args(["api", "graphql", "-f", &format!("query={query}")])
            .args(["-F", &format!("owner={owner}")])
            .args(["-F", &format!("repo={repo}")])
            .args(["-F", &format!("number={pr_number}")]);

        if let Some(cursor) = thread_cursor.as_deref() {
            command.args(["-F", &format!("threadCursor={cursor}")]);
        }

        let output = command.output().context("Failed to run `gh api graphql`")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "Failed to fetch PR review comments: {}",
                stderr.trim()
            ));
        }

        let parsed: ReviewThreadPageResponse =
            serde_json::from_slice(&output.stdout).context("Failed to parse graphql output")?;
        let pr = parsed
            .data
            .repository
            .and_then(|r| r.pull_request)
            .ok_or_else(|| anyhow!("PR not found in graphql response"))?;

        let has_next_page = pr.review_threads.page_info.has_next_page;
        thread_cursor = pr.review_threads.page_info.end_cursor.clone();
        review_threads.extend(pr.review_threads.nodes);

        if !has_next_page {
            break;
        }
    }

    let mut threads = Vec::new();
    for thread in review_threads {
        let mut comment_nodes = thread.comments.nodes;
        let mut has_next_comment_page = thread.comments.page_info.has_next_page;
        let mut comment_cursor = thread.comments.page_info.end_cursor.clone();
        let thread_id = thread.id;
        let is_resolved = thread.is_resolved;

        while has_next_comment_page {
            let thread_id = thread_id.as_deref().ok_or_else(|| {
                anyhow!("Review thread id missing for paginated graphql response")
            })?;
            let mut command = Command::new("gh");
            command
                .args(["api", "graphql", "-f", &format!("query={comments_query}")])
                .args(["-F", &format!("threadId={thread_id}")]);

            if let Some(cursor) = comment_cursor.as_deref() {
                command.args(["-F", &format!("commentsCursor={cursor}")]);
            }

            let output = command.output().context("Failed to run `gh api graphql`")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(anyhow!(
                    "Failed to fetch PR review comments: {}",
                    stderr.trim()
                ));
            }

            let parsed: ReviewThreadCommentsPageResponse =
                serde_json::from_slice(&output.stdout).context("Failed to parse graphql output")?;
            let comments = parsed
                .data
                .node
                .map(|node| node.comments)
                .ok_or_else(|| anyhow!("Review thread not found in graphql response"))?;

            has_next_comment_page = comments.page_info.has_next_page;
            comment_cursor = comments.page_info.end_cursor.clone();
            comment_nodes.extend(comments.nodes);
        }

        let mut comments: Vec<PrReviewComment> = comment_nodes
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
            is_resolved,
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

/// Merge a PR through the GitHub CLI.
pub fn merge_pr(pr_number: u64, head_oid: Option<&str>, method_flag: Option<&str>) -> Result<()> {
    let pr_number = pr_number.to_string();
    let mut command = Command::new("gh");
    command.args(["pr", "merge", &pr_number]);
    if let Some(method_flag) = method_flag {
        command.arg(method_flag);
    }
    if let Some(head_oid) = head_oid {
        command.args(["--match-head-commit", head_oid]);
    }
    let output = command.output().context("Failed to run `gh pr merge`")?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            "gh pr merge exited with a non-zero status".to_string()
        };
        Err(anyhow!("Failed to merge PR #{}: {}", pr_number, detail))
    }
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

/// Delete a branch on the remote (e.g. after its PR was merged). A missing
/// remote branch is treated as success so a re-run or a server-side
/// auto-delete-on-merge setting does not turn into an error.
pub fn delete_remote_branch(repo: &git2::Repository, remote: &str, branch: &str) -> Result<()> {
    let output = crate::repository::git_command(repo)
        .args(["push", remote, "--delete", branch])
        .output()
        .with_context(|| format!("Failed to run `git push {remote} --delete {branch}`"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    // GitHub deletes the head branch itself when merged with
    // auto-delete-on-merge, so the ref may already be gone. Match only git's
    // specific already-deleted message, not any "does not exist" (which also
    // covers a wrong remote or missing repository — real errors to surface).
    if stderr.contains("remote ref does not exist") {
        return Ok(());
    }

    Err(anyhow!(
        "Failed to delete remote branch '{}' on '{}': {}",
        branch,
        remote,
        stderr.trim()
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn page(json: &str) -> PrConnection {
        serde_json::from_str(json).unwrap()
    }

    const LOCAL: &str = r#"{"number":42,"headRefName":"feature","isCrossRepository":false,"baseRefName":"main","isDraft":true,"author":{"login":"me"},"title":"Local","body":"Body","url":"https://github.com/o/r/pull/42","labels":{"nodes":[{"name":"bug"}]},"reviewRequests":{"nodes":[{"requestedReviewer":{"login":"alice"}},{"requestedReviewer":{}}]}}"#;
    const FORK: &str = r#"{"number":99,"headRefName":"feature","isCrossRepository":true,"baseRefName":"main","isDraft":false,"author":{"login":"contributor"},"title":"Fork","body":"","url":"https://github.com/o/r/pull/99","labels":{"nodes":[]},"reviewRequests":{"nodes":[]}}"#;

    #[test]
    fn open_pr_nodes_keep_user_reviewers_labels_and_draft() {
        let mut map = HashMap::new();
        add_open_pr_nodes(
            &mut map,
            "feature",
            page(&format!(r#"{{"nodes":[{LOCAL}]}}"#)).nodes,
        );
        let pr = &map["feature"];
        assert_eq!(pr.number, 42);
        assert!(pr.is_draft);
        assert_eq!(pr.author_login.as_deref(), Some("me"));
        assert_eq!(pr.labels, ["bug"]);
        // The team reviewer has no login and is left out.
        assert_eq!(pr.reviewers, ["alice"]);
    }

    #[test]
    fn open_pr_nodes_prefer_this_repository_over_a_fork() {
        for nodes in [format!("[{FORK},{LOCAL}]"), format!("[{LOCAL},{FORK}]")] {
            let mut map = HashMap::new();
            add_open_pr_nodes(
                &mut map,
                "feature",
                page(&format!(r#"{{"nodes":{nodes}}}"#)).nodes,
            );
            assert_eq!(map["feature"].number, 42, "nodes: {nodes}");
        }
        let mut map = HashMap::new();
        add_open_pr_nodes(
            &mut map,
            "feature",
            page(&format!(r#"{{"nodes":[{FORK}]}}"#)).nodes,
        );
        assert_eq!(
            map["feature"].number, 99,
            "a fork PR claims a name nothing else has"
        );
    }

    #[test]
    fn open_pr_nodes_match_the_head_exactly() {
        let mut map = HashMap::new();
        add_open_pr_nodes(
            &mut map,
            "Feature",
            page(&format!(r#"{{"nodes":[{LOCAL}]}}"#)).nodes,
        );
        assert!(map.is_empty());
    }

    #[test]
    fn listed_review_requests_read_flattened_and_nested_logins() {
        let requests: Vec<PrReviewRequest> = serde_json::from_str(
            r#"[{"__typename":"User","login":"bob"},{"requestedReviewer":{"login":"alice"}},{"__typename":"Team","name":"t","slug":"t"}]"#,
        )
        .unwrap();
        let logins: Vec<_> = requests
            .into_iter()
            .filter_map(PrReviewRequest::login)
            .collect();
        assert_eq!(logins, ["bob", "alice"]);
    }

    #[test]
    fn repository_parts_keep_case_and_identity_lowercases() {
        assert_eq!(
            repository_parts("https://github.example.com/Owner/Repo"),
            Some(("github.example.com".into(), "Owner".into(), "Repo".into()))
        );
        assert_eq!(
            repository_identity("git@github.com:Owner/Repo.git").as_deref(),
            Some("github.com/owner/repo")
        );
    }
}
