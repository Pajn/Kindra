use anyhow::{Result, anyhow};
use git2::{Commit, Oid, Repository};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub struct StackBranch {
    pub name: String,
    pub id: Oid,
}

/// What a stack branch is stacked on: another branch of the stack, or the
/// commit the stack is measured from when no branch lies below it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StackParent {
    Base(Oid),
    Branch { name: String, id: Oid },
}

impl StackParent {
    /// The commit the branch's own commits start above.
    pub fn id(&self) -> Oid {
        match self {
            Self::Base(id) | Self::Branch { id, .. } => *id,
        }
    }

    /// The parent branch, or `None` when the branch sits on the base.
    pub fn branch_name(&self) -> Option<&str> {
        match self {
            Self::Base(_) => None,
            Self::Branch { name, .. } => Some(name),
        }
    }
}

#[derive(Clone, Debug)]
pub struct StackCommit {
    pub commit_id: Oid,
    pub branch_name: String,
    pub position: (usize, usize),
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct SyncBoundary {
    pub old_base: Option<Oid>,
    pub merged_branches: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ReorderPlan {
    pub ordered_sub_stack: Vec<StackBranch>,
    pub remaining_branches: Vec<String>,
    pub new_base_map: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct GraphReorderPlan {
    pub remaining_branches: Vec<String>,
    pub parent_id_map: HashMap<String, String>,
    pub new_base_map: HashMap<String, String>,
}

struct TargetPathHistory {
    /// The history holds the target commits not reachable from this commit.
    floor: Oid,
    commits: Vec<TargetPathCommit>,
    /// Ancestry among `commits`, for a history loaded without a pathspec.
    ancestry: Option<Ancestry>,
}

struct TargetPathCommit {
    id: Oid,
    changed_paths: HashSet<String>,
}

impl TargetPathHistory {
    /// Indexes the target commits after `floor` that touch `paths`, or every
    /// commit after it when `paths` is empty. Only commits after a branch's base
    /// can carry its changes: anything reachable from the base predates the
    /// branch, and walking it would scan the whole of the target's history.
    fn load(repo: &Repository, floor: Oid, target_tip: Oid, paths: &[String]) -> Result<Self> {
        let unfiltered = paths.is_empty();
        let mut log = crate::repository::git_command(repo);
        // Names are matched against real tree paths, so they are read NUL-framed
        // and unquoted, and the touched paths are matched literally, not as globs.
        log.args(["--literal-pathspecs", "-c", "core.quotePath=false"])
            .arg("log")
            .arg("-z")
            .arg("--format=__KINDRA_COMMIT__%H %P")
            .arg("--name-only")
            .arg("--no-renames")
            .arg("--no-ext-diff");
        if unfiltered {
            // Parents before children when reversed, for building the ancestry.
            log.arg("--topo-order");
        }
        let output = log
            .arg(format!("{floor}..{target_tip}"))
            .arg("--")
            .args(paths)
            .current_dir(repo_root(repo)?)
            .output()?;

        if !output.status.success() {
            return Err(anyhow!(
                "git log failed while indexing target history for sync containment checks."
            ));
        }

        let mut commits = Vec::new();
        let mut parents = Vec::new();
        let mut current: Option<TargetPathCommit> = None;

        // Each commit is its NUL-terminated header, then its paths, each
        // NUL-terminated, the first preceded by one newline.
        let mut first_path = false;
        for field in output.stdout.split(|&b| b == 0) {
            if let Some(header) = field.strip_prefix(b"__KINDRA_COMMIT__".as_slice()) {
                if let Some(commit) = current.take() {
                    commits.push(commit);
                }
                let header = std::str::from_utf8(header)?;
                let mut ids = header.split_whitespace().map(Oid::from_str);
                let id = ids
                    .next()
                    .ok_or_else(|| anyhow!("git log printed a commit without an id."))??;
                parents.push(ids.collect::<Result<Vec<_>, _>>()?);
                current = Some(TargetPathCommit {
                    id,
                    changed_paths: HashSet::new(),
                });
                first_path = true;
                continue;
            }

            let path = match field.strip_prefix(b"\n".as_slice()) {
                Some(path) if first_path => path,
                _ => field,
            };
            first_path = false;
            // Touched paths are always UTF-8, so a path that is not can never
            // complete a covering set; skipping it cannot claim a false match.
            let (Some(commit), Ok(path)) = (current.as_mut(), std::str::from_utf8(path)) else {
                continue;
            };
            if !path.is_empty() {
                commit.changed_paths.insert(path.to_string());
            }
        }

        if let Some(commit) = current {
            commits.push(commit);
        }

        // A pathspec rewrites parents to the commits that touch it, so only an
        // unfiltered history describes real ancestry.
        let ancestry = if unfiltered {
            Ancestry::build(&commits, &parents)
        } else {
            None
        };
        Ok(Self {
            floor,
            commits,
            ancestry,
        })
    }

    /// Commits after `base_id` that touch every one of `touched_paths`. A history
    /// shared from a lower floor also holds commits the base already contains;
    /// those predate the range and are dropped.
    fn covering_commits(
        &self,
        repo: &Repository,
        base_id: Oid,
        touched_paths: &[String],
    ) -> Result<Vec<Oid>> {
        let mut covering = Vec::new();
        for commit in &self.commits {
            let covers = commit.changed_paths.len() >= touched_paths.len()
                && touched_paths
                    .iter()
                    .all(|path| commit.changed_paths.contains(path));
            if !covers {
                continue;
            }
            if self.floor != base_id && self.base_contains(repo, base_id, commit.id)? {
                continue;
            }
            covering.push(commit.id);
        }
        Ok(covering)
    }

    /// Whether `base_id`, a commit of the target, contains `commit`, one of
    /// this history's commits.
    fn base_contains(&self, repo: &Repository, base_id: Oid, commit: Oid) -> Result<bool> {
        if commit == base_id {
            return Ok(true);
        }
        match &self.ancestry {
            // A base outside the history is reachable from the floor, and no
            // commit of the history is, so it cannot contain one.
            Some(ancestry) => Ok(ancestry.is_ancestor(commit, base_id).unwrap_or(false)),
            None => Ok(repo.graph_descendant_of(base_id, commit)?),
        }
    }
}

/// Which commits of a history reach which, as one bitset of ancestors per
/// commit. Answers in constant time what would otherwise be a graph walk per
/// question, which libgit2 cannot shortcut with a split commit-graph.
struct Ancestry {
    index: HashMap<Oid, usize>,
    ancestors: Vec<Vec<u64>>,
}

impl Ancestry {
    /// Beyond this many commits the bitsets cost more memory than the walks
    /// they save are worth (8192 commits take 8 MiB).
    const MAX_COMMITS: usize = 8192;

    /// `commits` in topological order, children first, with `parents[i]` the
    /// parents of `commits[i]`. Parents outside the history are ignored.
    fn build(commits: &[TargetPathCommit], parents: &[Vec<Oid>]) -> Option<Self> {
        if commits.len() > Self::MAX_COMMITS {
            return None;
        }
        let index: HashMap<Oid, usize> = commits
            .iter()
            .enumerate()
            .map(|(i, commit)| (commit.id, i))
            .collect();
        let words = commits.len().div_ceil(64);
        let mut ancestors = vec![vec![0u64; words]; commits.len()];
        for i in (0..commits.len()).rev() {
            ancestors[i][i / 64] |= 1 << (i % 64);
            for parent in &parents[i] {
                let Some(&p) = index.get(parent) else {
                    continue;
                };
                // Topological order puts every parent after its children, so its
                // ancestors are already complete; anything else is not trusted.
                if p <= i {
                    return None;
                }
                let (head, tail) = ancestors.split_at_mut(p);
                for (word, bits) in head[i].iter_mut().zip(&tail[0]) {
                    *word |= bits;
                }
            }
        }
        Some(Self { index, ancestors })
    }

    /// Whether `ancestor` is `of` or reachable from it, or `None` when either
    /// is outside the history.
    fn is_ancestor(&self, ancestor: Oid, of: Oid) -> Option<bool> {
        let a = *self.index.get(&ancestor)?;
        let o = *self.index.get(&of)?;
        Some(self.ancestors[o][a / 64] & (1 << (a % 64)) != 0)
    }
}

/// Reachability answers that [`find_sync_boundary`] needs for every local branch
/// in the repository.
///
/// None of them depend on which branch's boundary is being computed, only on
/// the stack's merge base and its upstream. Computing them once per sync rather
/// than once per branch is what stops a tree sync from costing
/// branches × local branches graph walks.
#[derive(Default)]
pub struct SyncScanCache {
    /// The `(merge_base, upstream_id, upstream_name)` used for candidate filtering.
    key: Option<(Oid, Oid, String)>,
    /// Local branches that sit above the merge base, so could be in a lineage.
    candidates: Vec<(String, Oid)>,
    /// Whether upstream already contains a branch tip.
    in_upstream: HashMap<Oid, bool>,
    /// Paths a commit range touches, by `(base, tip)`. The lineages of a tree
    /// stack overlap heavily, so the same ranges are asked for repeatedly.
    touched_paths: HashMap<(Oid, Oid), Arc<Vec<String>>>,
    /// Target history indexed by the base, target tip and exact path set it was loaded for.
    histories: HashMap<(Oid, Oid, Vec<String>), Arc<TargetPathHistory>>,
    /// A floor below every base about to be scanned against a target, set by
    /// [`SyncScanCache::share_history`]; the history above it loads on first need.
    shared_floor: Option<(Oid, Oid)>,
    /// The unfiltered target history above `shared_floor`, once loaded. Shared
    /// with the caches of [`SyncScanCache::worker`], so it loads once for all
    /// of them.
    shared_history: Arc<Mutex<Option<Arc<TargetPathHistory>>>>,
}

impl SyncScanCache {
    /// Local branches that could belong to a stack lineage, with everything
    /// that does not depend on the lineage's tip already filtered out.
    fn candidates(
        &mut self,
        repo: &Repository,
        merge_base: Oid,
        upstream_id: Oid,
        upstream_name: &str,
    ) -> Result<&[(String, Oid)]> {
        let key = (merge_base, upstream_id, upstream_name.to_string());
        if self.key.as_ref() == Some(&key) {
            return Ok(&self.candidates);
        }

        let upstream_ref_name = repo
            .resolve_reference_from_short_name(upstream_name)
            .ok()
            .and_then(|r| r.name().map(|s| s.to_string()));

        // A lineage runs from the merge base up, so a branch below it can
        // never be in one, whichever tip is being asked about.
        let above_merge_base = local_branches_by_ancestry(repo, &[("--contains", merge_base)])?;
        let mut candidates = Vec::new();
        for branch in local_branches_with_upstreams(repo)? {
            if branch.name == upstream_name {
                continue;
            }

            if let Some(ref ref_name) = upstream_ref_name
                && (branch.refname == *ref_name || branch.upstream.as_ref() == Some(ref_name))
            {
                continue;
            }

            if !above_merge_base.contains(&branch.name) {
                continue;
            }
            candidates.push((branch.name, branch.tip));
        }

        self.candidates = candidates;
        self.in_upstream.clear();
        self.key = Some(key);
        Ok(&self.candidates)
    }

    fn in_upstream(&mut self, repo: &Repository, upstream_id: Oid, id: Oid) -> Result<bool> {
        if let Some(known) = self.in_upstream.get(&id) {
            return Ok(*known);
        }
        let contained = upstream_id == id || repo.graph_descendant_of(upstream_id, id)?;
        self.in_upstream.insert(id, contained);
        Ok(contained)
    }

    fn touched_paths(
        &mut self,
        repo: &Repository,
        base_id: Oid,
        tip: Oid,
    ) -> Result<Arc<Vec<String>>> {
        if let Some(known) = self.touched_paths.get(&(base_id, tip)) {
            return Ok(Arc::clone(known));
        }
        let paths = Arc::new(range_touched_paths(repo, base_id, tip)?);
        self.touched_paths
            .insert((base_id, tip), Arc::clone(&paths));
        Ok(paths)
    }

    fn history(
        &mut self,
        repo: &Repository,
        base_id: Oid,
        target_tip: Oid,
        paths: &[String],
    ) -> Result<Arc<TargetPathHistory>> {
        if paths.is_empty() {
            return Ok(Arc::new(TargetPathHistory {
                floor: base_id,
                commits: Vec::new(),
                ancestry: None,
            }));
        }
        if let Some((target, floor)) = self.shared_floor
            && target == target_tip
        {
            // Held while loading, so workers asking at once wait for one load.
            let mut shared = self
                .shared_history
                .lock()
                .map_err(|_| anyhow!("A thread loading the target history panicked."))?;
            if let Some(shared) = &*shared {
                return Ok(Arc::clone(shared));
            }
            let history = Arc::new(TargetPathHistory::load(repo, floor, target_tip, &[])?);
            *shared = Some(Arc::clone(&history));
            return Ok(history);
        }
        let key = (base_id, target_tip, paths.to_vec());
        if let Some(known) = self.histories.get(&key) {
            return Ok(Arc::clone(known));
        }
        let history = Arc::new(TargetPathHistory::load(repo, base_id, target_tip, paths)?);
        self.histories.insert(key, Arc::clone(&history));
        Ok(history)
    }

    /// A cache for another thread scanning against the same target, sharing
    /// this one's shared history but none of its per-range answers.
    fn worker(&self) -> Self {
        Self {
            shared_floor: self.shared_floor,
            shared_history: Arc::clone(&self.shared_history),
            ..Self::default()
        }
    }

    /// Serves every later history request against `target_tip` from one walk of
    /// the target above the common ancestor of `bases`, rather than one
    /// path-limited walk per range. Every base must be an ancestor of the target.
    fn share_history(&mut self, repo: &Repository, target_tip: Oid, bases: &[Oid]) {
        if bases.len() < 2 {
            return;
        }
        // Without a common ancestor there is no single floor; keep per-range walks.
        if let Ok(Some(floor)) = octopus_merge_base(repo, bases) {
            self.shared_floor = Some((target_tip, floor));
            self.shared_history = Arc::default();
        }
    }
}

pub fn find_sync_boundary(
    repo: &Repository,
    top_branch: &str,
    upstream_name: &str,
    stack_branches: &[StackBranch],
) -> Result<SyncBoundary> {
    find_sync_boundary_cached(
        repo,
        top_branch,
        upstream_name,
        stack_branches,
        &mut SyncScanCache::default(),
    )
}

/// [`find_sync_boundary`] reusing `cache` across the branches of one stack.
pub fn find_sync_boundary_cached(
    repo: &Repository,
    top_branch: &str,
    upstream_name: &str,
    stack_branches: &[StackBranch],
    cache: &mut SyncScanCache,
) -> Result<SyncBoundary> {
    let top_id = repo.revparse_single(top_branch)?.id();
    let upstream_id = repo.revparse_single(upstream_name)?.id();
    let merge_base = resolve_merge_base(repo, top_id, upstream_id)?;
    let lineage = ordered_stack_lineage(repo, top_id, stack_branches)?;

    let mut merged_branches = HashSet::new();
    let mut branch_cutoff = merge_base;
    for branch in lineage.iter().take(lineage.len().saturating_sub(1)) {
        if !branch_segment_integrated(repo, branch_cutoff, branch.id, upstream_id, cache)? {
            break;
        }

        merged_branches.insert(branch.name.clone());
        branch_cutoff = branch.id;
    }

    // Each candidate prefix is compared by its change relative to the cutoff,
    // which only commits descending from the cutoff have. A branch that merged
    // its parent reaches the cutoff through a merge, not its first parent.
    let first_parent_chain = first_parent_chain_descending_from(repo, branch_cutoff, top_id)?;
    let prefix_touched_paths = first_parent_chain
        .iter()
        .map(|&commit_id| cache.touched_paths(repo, branch_cutoff, commit_id))
        .collect::<Result<Vec<_>>>()?;
    let mut union_paths = prefix_touched_paths
        .iter()
        .flat_map(|paths| paths.iter().cloned())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    union_paths.sort();
    // Loaded on first need: most commits are settled by graph or tree checks.
    let mut target_history = None;
    let mut prefix_end: isize = -1;

    for (idx, (&commit_id, touched_paths)) in first_parent_chain
        .iter()
        .zip(prefix_touched_paths.iter())
        .enumerate()
    {
        let merged_by_graph =
            repo.graph_descendant_of(upstream_id, commit_id)? || upstream_id == commit_id;
        if merged_by_graph {
            prefix_end = idx as isize;
            continue;
        }

        let present = match range_changes_settled_by_trees(
            repo,
            branch_cutoff,
            commit_id,
            upstream_id,
            touched_paths,
        )? {
            Some(present) => present,
            None => {
                let history = match &target_history {
                    Some(history) => Arc::clone(history),
                    None => Arc::clone(target_history.insert(cache.history(
                        repo,
                        branch_cutoff,
                        upstream_id,
                        &union_paths,
                    )?)),
                };
                range_changes_present_in_history(
                    repo,
                    branch_cutoff,
                    commit_id,
                    touched_paths,
                    &history,
                )?
            }
        };
        if present {
            prefix_end = idx as isize;
        }
    }

    // Candidates are the local branches above the merge base; the rest can
    // never be in this lineage. Only the question that depends on `top_id` is
    // left to ask per branch.
    let candidates = cache
        .candidates(repo, merge_base, upstream_id, upstream_name)?
        .to_vec();
    for (name, id) in candidates {
        // Is this branch part of the stack? An ancestor of top_branch, and (via
        // the cache) a descendant of merge_base.
        if !(repo.graph_descendant_of(top_id, id)? || top_id == id) {
            continue;
        }

        // A branch already recorded as merged by content stays recorded, so only
        // graph containment can add to the set here.
        if cache.in_upstream(repo, upstream_id, id)? {
            merged_branches.insert(name);
        }
    }

    let first_unmerged_idx = (prefix_end + 1) as usize;
    if first_unmerged_idx >= first_parent_chain.len() {
        // Everything the top branch adds is already upstream, by graph or by
        // content, so it is merged on the same evidence the lower branches were
        // recorded on, whether or not upstream reaches its tip. A range whose
        // net change is empty (an edit reverted on the branch) passes the scan
        // with nothing to rebase, but that says nothing about a merge, so it
        // does not count. An empty chain means the tip is the cutoff itself:
        // the merge base, or the tip of a lower branch recorded as merged.
        let has_content_evidence = prefix_touched_paths
            .last()
            .is_none_or(|paths| !paths.is_empty());
        if has_content_evidence {
            merged_branches.insert(top_branch.to_string());
        }
        let mut merged_branches = merged_branches.into_iter().collect::<Vec<_>>();
        merged_branches.sort();
        return Ok(SyncBoundary {
            old_base: None,
            merged_branches,
        });
    }

    // Rebasing from the cutoff, not the first commit's first parent, keeps the
    // branch's own commits below a merge of its parent in the replay.
    let old_base = match first_unmerged_idx.checked_sub(1) {
        Some(last_merged) => first_parent_chain[last_merged],
        None => branch_cutoff,
    };

    let mut merged_branches = merged_branches.into_iter().collect::<Vec<_>>();
    merged_branches.sort();

    Ok(SyncBoundary {
        old_base: Some(old_base),
        merged_branches,
    })
}

pub fn plan_descendant_reorder(
    repo: &Repository,
    current_branch_name: &str,
    target_branch_name: &str,
    all_branches_in_stack: &[StackBranch],
    merge_base: Oid,
    upstream_name: &str,
) -> Result<Option<ReorderPlan>> {
    if current_branch_name == target_branch_name {
        return Ok(None);
    }

    let mut sub_stack = Vec::new();
    collect_descendants(
        repo,
        current_branch_name,
        all_branches_in_stack,
        &mut sub_stack,
    )?;

    if !sub_stack
        .iter()
        .any(|branch| branch.name == target_branch_name)
    {
        return Ok(None);
    }

    sort_branches_topologically(repo, &mut sub_stack)?;
    let current_parent =
        find_parent_in_stack(repo, current_branch_name, all_branches_in_stack, merge_base)?;
    let current_parent_id = current_parent.id();
    // A tree rotation cannot preserve merge edges using branch-by-branch rebases.
    // The parent and its ancestors are unchanged, so exclude them from validation.
    for branch in &sub_stack {
        for id in collect_first_parent_chain(repo, current_parent_id, branch.id)? {
            if repo.find_commit(id)?.parent_count() > 1 {
                return Err(anyhow!(
                    "Cannot reorder because the affected subtree is forked and merged. Tree reordering requires single-parent commits."
                ));
            }
        }
    }

    let current_parent = current_parent
        .branch_name()
        .unwrap_or(upstream_name)
        .to_string();

    let parents = find_parents_in_stack(repo, all_branches_in_stack, merge_base)?;
    let mut new_base_map = HashMap::new();
    for branch in &sub_stack {
        let parent = &parents[&branch.name];
        new_base_map.insert(
            branch.name.clone(),
            parent
                .branch_name()
                .map_or_else(|| parent.id().to_string(), str::to_string),
        );
    }

    // Preserve the existing linear rotation, stopping at a fork instead of
    // choosing an arbitrary leaf. Only these two edges change; side paths keep
    // their original parents on both sides of the rotation.
    let mut attachment = target_branch_name.to_string();
    loop {
        let children = sub_stack
            .iter()
            .filter(|branch| new_base_map.get(&branch.name) == Some(&attachment))
            .collect::<Vec<_>>();
        if children.len() != 1 {
            break;
        }
        attachment = children[0].name.clone();
    }
    new_base_map.insert(target_branch_name.to_string(), current_parent.clone());
    new_base_map.insert(current_branch_name.to_string(), attachment);
    let order_hint = sub_stack
        .iter()
        .enumerate()
        .map(|(idx, branch)| (branch.name.clone(), idx))
        .collect();
    let remaining_branches =
        topologically_sort_edited_graph(&new_base_map, &current_parent, &order_hint)?;

    Ok(Some(ReorderPlan {
        ordered_sub_stack: sub_stack,
        remaining_branches,
        new_base_map,
    }))
}

pub fn collect_stack_component(
    repo: &Repository,
    current_branch_name: &str,
    merge_base: Oid,
    upstream_id: Oid,
    upstream_name: &str,
) -> Result<Vec<StackBranch>> {
    let local_branches = local_branch_tips(repo)?;
    let mut candidates = Vec::new();
    // At or above the merge base and not yet in upstream, for every branch in
    // one query.
    let unmerged_above_merge_base = local_branches_by_ancestry(
        repo,
        &[("--contains", merge_base), ("--no-merged", upstream_id)],
    )?;

    for (name, id) in local_branches {
        if name == upstream_name {
            continue;
        }

        if unmerged_above_merge_base.contains(&name) {
            candidates.push(StackBranch {
                name: name.to_string(),
                id,
            });
        }
    }

    if !candidates
        .iter()
        .any(|branch| branch.name == current_branch_name)
    {
        return Err(anyhow!(
            "Branch '{}' not found in the current stack component.",
            current_branch_name
        ));
    }

    let mut adjacency: HashMap<String, Vec<String>> = candidates
        .iter()
        .map(|branch| (branch.name.clone(), Vec::new()))
        .collect();

    // Every candidate uses the same tips and base. Build their ancestry once,
    // including unrelated trunk forks, instead of walking it once per branch.
    let parents = find_parents_in_stack(repo, &candidates, merge_base)?;
    for branch in &candidates {
        if let Some(parent_name) = parents[&branch.name].branch_name() {
            adjacency
                .get_mut(&branch.name)
                .expect("branch adjacency entry must exist")
                .push(parent_name.to_string());
            adjacency
                .get_mut(parent_name)
                .expect("parent adjacency entry must exist")
                .push(branch.name.clone());
        }
    }

    let mut queue = VecDeque::from([current_branch_name.to_string()]);
    let mut visited = HashSet::new();
    while let Some(branch_name) = queue.pop_front() {
        if !visited.insert(branch_name.clone()) {
            continue;
        }
        if let Some(neighbors) = adjacency.get(&branch_name) {
            for neighbor in neighbors {
                if !visited.contains(neighbor) {
                    queue.push_back(neighbor.clone());
                }
            }
        }
    }

    let mut component = candidates
        .into_iter()
        .filter(|branch| visited.contains(&branch.name))
        .collect::<Vec<_>>();
    sort_branches_topologically(repo, &mut component)?;
    Ok(component)
}

pub fn current_parent_name_map(
    repo: &Repository,
    branches: &[StackBranch],
    merge_base: Oid,
    upstream_name: &str,
) -> Result<HashMap<String, String>> {
    // One shared ancestry walk for the whole set, rather than rebuilding it per
    // branch: the parent of every branch is the same computation, and the walk is
    // what costs.
    let parents = find_parents_in_stack(repo, branches, merge_base)?;
    Ok(parents
        .into_iter()
        .map(|(name, parent)| {
            let parent_name = parent.branch_name().unwrap_or(upstream_name).to_string();
            (name, parent_name)
        })
        .collect())
}

pub fn plan_graph_reorder(
    repo: &Repository,
    branches: &[StackBranch],
    merge_base: Oid,
    upstream_name: &str,
    edited_parent_map: &HashMap<String, String>,
) -> Result<GraphReorderPlan> {
    let expected_names = branches
        .iter()
        .map(|branch| branch.name.clone())
        .collect::<HashSet<_>>();

    if edited_parent_map.len() != branches.len() {
        return Err(anyhow!(
            "Edited branch graph is incomplete. Every branch must appear exactly once."
        ));
    }

    for branch in branches {
        let Some(parent_name) = edited_parent_map.get(&branch.name) else {
            return Err(anyhow!(
                "Branch '{}' is missing from the edited graph.",
                branch.name
            ));
        };

        if parent_name == &branch.name {
            return Err(anyhow!(
                "Branch '{}' cannot list itself as its parent.",
                branch.name
            ));
        }

        if parent_name != upstream_name && !expected_names.contains(parent_name) {
            return Err(anyhow!(
                "Branch '{}' has unknown parent '{}'.",
                branch.name,
                parent_name
            ));
        }
    }

    let order_hint = branches
        .iter()
        .enumerate()
        .map(|(idx, branch)| (branch.name.clone(), idx))
        .collect::<HashMap<_, _>>();
    let remaining_branches =
        topologically_sort_edited_graph(edited_parent_map, upstream_name, &order_hint)?;

    let parents = find_parents_in_stack(repo, branches, merge_base)?;
    let mut parent_id_map = HashMap::new();
    let mut new_base_map = HashMap::new();
    for branch in branches {
        parent_id_map.insert(branch.name.clone(), parents[&branch.name].id().to_string());
        new_base_map.insert(
            branch.name.clone(),
            edited_parent_map
                .get(&branch.name)
                .expect("edited parent map already validated")
                .clone(),
        );
    }

    Ok(GraphReorderPlan {
        remaining_branches,
        parent_id_map,
        new_base_map,
    })
}

fn topologically_sort_edited_graph(
    edited_parent_map: &HashMap<String, String>,
    upstream_name: &str,
    order_hint: &HashMap<String, usize>,
) -> Result<Vec<String>> {
    let mut indegree = edited_parent_map
        .keys()
        .map(|branch| (branch.clone(), 0usize))
        .collect::<HashMap<_, _>>();
    let mut children = edited_parent_map
        .keys()
        .map(|branch| (branch.clone(), Vec::new()))
        .collect::<HashMap<_, Vec<String>>>();

    for (branch, parent) in edited_parent_map {
        if parent == upstream_name {
            continue;
        }
        *indegree
            .get_mut(branch)
            .expect("indegree entry must exist for branch") += 1;
        children
            .get_mut(parent)
            .expect("child list must exist for parent branch")
            .push(branch.clone());
    }

    let mut ready = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(branch, _)| branch.clone())
        .collect::<Vec<_>>();
    ready.sort_by_key(|branch| order_hint.get(branch).copied().unwrap_or(usize::MAX));
    let mut ready = VecDeque::from(ready);

    let mut sorted = Vec::with_capacity(edited_parent_map.len());
    while let Some(branch) = ready.pop_front() {
        sorted.push(branch.clone());

        if let Some(child_names) = children.get(&branch) {
            for child in child_names {
                let degree = indegree
                    .get_mut(child)
                    .expect("indegree entry must exist for child branch");
                *degree -= 1;
                if *degree == 0 {
                    ready.push_back(child.clone());
                }
            }
            let mut ready_sorted = ready.into_iter().collect::<Vec<_>>();
            ready_sorted
                .sort_by_key(|candidate| order_hint.get(candidate).copied().unwrap_or(usize::MAX));
            ready = VecDeque::from(ready_sorted);
        }
    }

    if sorted.len() != edited_parent_map.len() {
        return Err(anyhow!(
            "Edited branch graph contains a cycle. Every branch must eventually trace back to '{}'.",
            upstream_name
        ));
    }

    Ok(sorted)
}

/// The commits on `tip`'s first-parent history from `ancestor_exclusive` (not
/// included) up to `tip`, oldest first. Fails when `ancestor_exclusive` is not
/// on that history, for example when the branch merged its parent instead of
/// being rebased onto it.
pub fn collect_first_parent_chain(
    repo: &Repository,
    ancestor_exclusive: Oid,
    tip: Oid,
) -> Result<Vec<Oid>> {
    if tip == ancestor_exclusive {
        return Ok(Vec::new());
    }
    let chain = first_parent_commits_above(repo, ancestor_exclusive, tip)?;
    let joins_ancestor = match chain.first() {
        Some(&oldest) => repo.find_commit(oldest)?.parent_ids().next() == Some(ancestor_exclusive),
        None => false,
    };
    if joins_ancestor {
        return Ok(chain);
    }
    if repo.graph_descendant_of(tip, ancestor_exclusive)? {
        return Err(anyhow!(
            "{} is not on the first-parent history of {}; the branch probably merged its parent instead of rebasing onto it.",
            ancestor_exclusive,
            tip
        ));
    }
    Err(anyhow!(
        "{} is not an ancestor of {}.",
        ancestor_exclusive,
        tip
    ))
}

/// The commits on `tip`'s first-parent history that `ancestor` does not
/// reach, oldest first. The walk stops where the first-parent history meets
/// `ancestor`'s history, so it never runs to the root of a related history.
fn first_parent_commits_above(repo: &Repository, ancestor: Oid, tip: Oid) -> Result<Vec<Oid>> {
    let mut walk = repo.revwalk()?;
    // Topological, so skewed commit dates cannot reorder the chain.
    walk.set_sorting(git2::Sort::TOPOLOGICAL)?;
    walk.simplify_first_parent()?;
    walk.push(tip)?;
    walk.hide(ancestor)?;
    let mut chain = walk.collect::<std::result::Result<Vec<_>, _>>()?;
    chain.reverse();
    Ok(chain)
}

/// The commits on `tip`'s first-parent history that descend from `ancestor`,
/// oldest first. This is [`collect_first_parent_chain`] when `ancestor` is on
/// that history. When the branch merged `ancestor` instead, the chain starts at
/// the first-parent commit that brought it in (a merge): the commits below it
/// have no change relative to `ancestor` of their own to compare, and rebasing
/// from `ancestor` replays them anyway.
fn first_parent_chain_descending_from(
    repo: &Repository,
    ancestor: Oid,
    tip: Oid,
) -> Result<Vec<Oid>> {
    if tip == ancestor {
        return Ok(Vec::new());
    }
    let mut chain = first_parent_commits_above(repo, ancestor, tip)?;
    if let Some(&oldest) = chain.first()
        && repo.find_commit(oldest)?.parent_ids().next() == Some(ancestor)
    {
        return Ok(chain);
    }
    // Descending from `ancestor` is monotone along a first-parent history, so
    // the first descendant can be found by bisection.
    let (mut low, mut high) = (0, chain.len());
    while low < high {
        let mid = low + (high - low) / 2;
        if repo.graph_descendant_of(chain[mid], ancestor)? {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    if low == chain.len() {
        return Err(anyhow!("{} is not an ancestor of {}.", ancestor, tip));
    }
    chain.drain(..low);
    Ok(chain)
}

/// A local branch that took another local branch's commits with a merge
/// instead of being rebased onto them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergedBranch {
    /// The branch holding the merge commit.
    pub merger: String,
    /// The branch whose commits the merge brought in.
    pub merged: String,
    /// Whether `merged`'s tip is among the merged commits, rather than `merged`
    /// having gone on after the merge.
    pub merged_tip_replayed: bool,
    /// Whether other branches are stacked on `merger`.
    pub merger_has_descendants: bool,
}

/// Finds a merge of another branch in the commits a `git rebase --update-refs`
/// of `base..head` replays (`base` excluded; `None` replays all of `head`).
///
/// Such a rebase drops the merge and replays the merged commits in line with
/// the merging branch's own, so their order mixes the two branches:
/// `--update-refs` then moves the merged branch to its replayed tip, on top of
/// commits of the branch that merged it, and a merged branch that went on
/// after the merge ends up with its commits copied into the other. A merge
/// whose merged side `base` or `onto` already reaches brings in nothing that is
/// replayed; this is what keeps a merged parent that is already integrated
/// upstream, or a merge of the trunk, out of it.
pub fn find_merged_branch_in_replay(
    repo: &Repository,
    base: Option<Oid>,
    head: Oid,
    onto: Option<Oid>,
) -> Result<Option<MergedBranch>> {
    let reaches = |from: Option<Oid>, id: Oid| -> Result<bool> {
        Ok(match from {
            Some(from) => from == id || repo.graph_descendant_of(from, id)?,
            None => false,
        })
    };
    let mut walk = repo.revwalk()?;
    walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;
    walk.push(head)?;
    if let Some(base) = base {
        walk.hide(base)?;
    }
    // Only loaded once a merge is found, which most replays never contain.
    let mut branches: Option<Vec<(String, Oid)>> = None;
    for merge in walk {
        let merge = merge?;
        let commit = repo.find_commit(merge)?;
        for merged_side in commit.parent_ids().skip(1) {
            if reaches(base, merged_side)? || reaches(onto, merged_side)? {
                continue;
            }
            let branches = match &mut branches {
                Some(branches) => branches,
                None => branches.insert(local_branch_tips(repo)?),
            };
            // The branch whose commits the merge brought in: preferably one whose
            // tip is among them (the nearest to the merge), otherwise the nearest
            // one that went on from them.
            let mut replayed: Option<(&str, Oid)> = None;
            let mut went_on: Option<(&str, Oid)> = None;
            for (name, tip) in branches.iter() {
                let tip = *tip;
                if tip == merge
                    || repo.graph_descendant_of(tip, merge)?
                    || reaches(base, tip)?
                    || reaches(onto, tip)?
                {
                    continue;
                }
                if tip == merged_side || repo.graph_descendant_of(merged_side, tip)? {
                    if match replayed {
                        None => true,
                        Some((_, best)) => repo.graph_descendant_of(tip, best)?,
                    } {
                        replayed = Some((name, tip));
                    }
                } else if repo.graph_descendant_of(tip, merged_side)?
                    && match went_on {
                        None => true,
                        Some((_, best)) => repo.graph_descendant_of(best, tip)?,
                    }
                {
                    went_on = Some((name, tip));
                }
            }
            let Some((merged, _)) = replayed.or(went_on) else {
                continue;
            };
            // The branch holding the merge: the nearest one at or above it.
            let mut merger: Option<(&str, Oid)> = None;
            for (name, tip) in branches.iter() {
                let tip = *tip;
                let holds_merge = tip == merge || repo.graph_descendant_of(tip, merge)?;
                let replayed_tip = tip == head || repo.graph_descendant_of(head, tip)?;
                if holds_merge
                    && replayed_tip
                    && match merger {
                        None => true,
                        Some((_, best)) => repo.graph_descendant_of(best, tip)?,
                    }
                {
                    merger = Some((name, tip));
                }
            }
            let (merger, merger_tip) = match merger {
                Some((name, tip)) => (name.to_string(), tip),
                None => (merge.to_string(), merge),
            };
            let mut merger_has_descendants = false;
            for (_, tip) in branches.iter() {
                if repo.graph_descendant_of(*tip, merger_tip)? {
                    merger_has_descendants = true;
                    break;
                }
            }
            return Ok(Some(MergedBranch {
                merger,
                merged: merged.to_string(),
                merged_tip_replayed: replayed.is_some(),
                merger_has_descendants,
            }));
        }
    }
    Ok(None)
}

/// Every local branch and its tip, sorted by name. Read with git, as
/// [`local_branches_with_upstreams`] explains; listing hundreds of branches
/// through libgit2 takes several times as long.
pub(crate) fn local_branch_tips(repo: &Repository) -> Result<Vec<(String, Oid)>> {
    let mut tips: Vec<_> = local_branches_with_upstreams(repo)?
        .into_iter()
        .map(|branch| (branch.name, branch.tip))
        .collect();
    tips.sort();
    Ok(tips)
}

pub fn collect_merged_local_branches(
    repo: &Repository,
    target_ref_name: &str,
    protected_branches: &[&str],
) -> Result<Vec<String>> {
    let target_id = repo.revparse_single(target_ref_name)?.id();
    let full_target_ref_name = repo
        .resolve_reference_from_short_name(target_ref_name)
        .ok()
        .and_then(|reference| reference.name().map(|name| name.to_string()));
    let target_name_for_comparison = full_target_ref_name.as_deref().unwrap_or(target_ref_name);
    let target_short_name = target_name_for_comparison
        .strip_prefix("refs/heads/")
        .or_else(|| target_name_for_comparison.strip_prefix("refs/remotes/"))
        .unwrap_or(target_name_for_comparison);
    let protected_branches = protected_branches
        .iter()
        .map(|name| name.to_string())
        .collect::<HashSet<_>>();

    let mut branches = Vec::new();
    for branch in local_branches_with_upstreams(repo)? {
        if protected_branches.contains(&branch.name) || branch.name == target_short_name {
            continue;
        }

        if let Some(upstream) = &branch.upstream {
            let upstream_short = upstream.strip_prefix("refs/remotes/").unwrap_or(upstream);
            if upstream_short == target_short_name {
                continue;
            }
        }

        branches.push((branch.name, branch.tip));
    }

    let mut merged_branches = Vec::new();
    // Branches here share ranges and path sets, so the memoised lookups pay off
    // across the loop even though the per-stack answers are not reused.
    let mut cache = SyncScanCache::default();
    let finding_bases = crate::timing::phase("merge bases");
    let tips_in_target = local_branches_by_ancestry(repo, &[("--merged", target_id)])?;
    let mut unmerged = Vec::new();
    for (name, branch_id) in branches {
        if tips_in_target.contains(&name) {
            merged_branches.push(name);
        } else {
            unmerged.push((name, branch_id));
        }
    }
    let tips = unmerged.iter().map(|&(_, tip)| tip).collect::<Vec<_>>();
    let merge_bases = merge_bases_with_target(repo, target_id, &tips)?;
    let unmerged_by_graph = unmerged
        .into_iter()
        .filter_map(|(name, tip)| Some((name, tip, *merge_bases.get(&tip)?)))
        .collect::<Vec<_>>();

    // Each range differs in its base, so per-range walks of the target rarely
    // repeat; one walk above all the bases answers every range instead.
    let bases = unmerged_by_graph
        .iter()
        .map(|&(_, _, merge_base)| merge_base)
        .collect::<Vec<_>>();
    cache.share_history(repo, target_id, &bases);
    drop(finding_bases);
    let _checking = crate::timing::phase("squash-merge checks");
    merged_branches.extend(ranges_present_in_target(
        repo,
        target_id,
        unmerged_by_graph,
        &cache,
    )?);

    merged_branches.sort();
    Ok(merged_branches)
}

/// The names among `ranges`, each a branch with its tip and merge base, whose
/// changes [`range_changes_present_in_target`] finds in `target`.
///
/// Each range is answered on its own, so they are spread over a few threads,
/// each with its own repository handle and a [`SyncScanCache::worker`]
/// of `cache`, which loads the shared target history once for all of them.
/// With hundreds of local branches these checks are most of a sync's scan.
fn ranges_present_in_target(
    repo: &Repository,
    target: Oid,
    ranges: Vec<(String, Oid, Oid)>,
    cache: &SyncScanCache,
) -> Result<Vec<String>> {
    // Below this many ranges, opening handles costs more than threads save.
    const MIN_RANGES_PER_THREAD: usize = 8;
    // libgit2 serialises reads of pack files across the whole process, so
    // beyond a handful of threads they mostly wait on each other.
    const MAX_THREADS: usize = 8;
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_THREADS)
        .min(ranges.len() / MIN_RANGES_PER_THREAD)
        .max(1);
    // Ranges are taken one at a time, so one slow range does not hold up a
    // share of the others behind it.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let scan = |repo: &Repository| -> Result<Vec<String>> {
        let mut cache = cache.worker();
        let mut present = Vec::new();
        while let Some((name, tip, base)) =
            ranges.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
        {
            if range_changes_present_in_target(repo, *base, *tip, target, &mut cache)? {
                present.push(name.clone());
            }
        }
        Ok(present)
    };
    if threads == 1 {
        return scan(repo);
    }

    // libgit2 handles cannot cross threads; each worker opens its own.
    let path = repo.path();
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| -> Result<Vec<String>> {
                    let repo = Repository::open(path)?;
                    scan(&repo)
                })
            })
            .collect();
        let mut present = Vec::new();
        for worker in workers {
            let found = worker
                .join()
                .map_err(|_| anyhow!("A thread checking merged branches panicked."))??;
            present.extend(found);
        }
        Ok(present)
    })
}

/// A local branch, its tip and the full name of its configured upstream.
struct LocalBranch {
    name: String,
    refname: String,
    tip: Oid,
    upstream: Option<String>,
}

/// Every local branch with its tip and upstream, from one `git for-each-ref`.
/// With hundreds of branches, libgit2 takes several times as long to list
/// them, and reads each branch's upstream from its config and the remote's
/// refspecs anew. Symbolic branch refs and names that are not UTF-8 are left
/// out, as libgit2's listing gives them no tip or name.
fn local_branches_with_upstreams(repo: &Repository) -> Result<Vec<LocalBranch>> {
    let output = crate::repository::git_command(repo)
        .args([
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(upstream)%00%(symref)",
            "refs/heads/",
        ])
        .output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "git for-each-ref failed while listing local branches: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut branches = Vec::new();
    for line in output.stdout.split(|&b| b == b'\n') {
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        let mut fields = line.split('\0');
        let (Some(refname), Some(tip), Some(upstream), Some(symref)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Some(name) = refname.strip_prefix("refs/heads/") else {
            continue;
        };
        if !symref.is_empty() {
            continue;
        }
        branches.push(LocalBranch {
            name: name.to_string(),
            refname: refname.to_string(),
            tip: Oid::from_str(tip)?,
            upstream: (!upstream.is_empty()).then(|| upstream.to_string()),
        });
    }
    Ok(branches)
}

/// Local branches whose tips pass every `(filter, commit)` pair: `--merged`
/// for tips that `commit` contains, `--no-merged` for tips it does not, and
/// `--contains` for tips that contain `commit`. Git ORs repeats of one kind,
/// so pass each kind at most once. One query answers for every branch, and
/// git reads the commit-graph, split chains included, where libgit2 would
/// walk the commits behind each branch in turn.
fn local_branches_by_ancestry(
    repo: &Repository,
    filters: &[(&str, Oid)],
) -> Result<HashSet<String>> {
    let mut command = crate::repository::git_command(repo);
    command.arg("for-each-ref");
    for (filter, commit) in filters {
        command.arg(format!("{filter}={commit}"));
    }
    let output = command
        .arg("--format=%(refname)")
        .arg("refs/heads/")
        .output()?;
    if !output.status.success() {
        let filters: Vec<String> = filters.iter().map(|(f, c)| format!("{f}={c}")).collect();
        return Err(anyhow!(
            "git for-each-ref {} failed while relating local branches.",
            filters.join(" ")
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("refs/heads/"))
        .map(str::to_string)
        .collect())
}

/// Run `command` with `input` on its stdin and collect its output.
fn output_with_stdin(
    mut command: std::process::Command,
    input: String,
) -> Result<std::process::Output> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("Failed to open git's stdin."))?;
    // Feed the input from another thread: with enough of it the output fills
    // the pipe before the input is fully written, and both ends would block.
    std::thread::scope(|scope| {
        let writer = scope.spawn(move || {
            use std::io::Write;
            stdin.write_all(input.as_bytes())
        });
        let output = child.wait_with_output();
        let written = writer
            .join()
            .map_err(|_| anyhow!("git input writer panicked"))?;
        // A closed pipe means git exited early; its status carries the error.
        if let Err(err) = written
            && err.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(err.into());
        }
        output.map_err(anyhow::Error::from)
    })
}

/// The merge base of each of `tips` with `target`, or no entry for a tip
/// that shares no history with it.
///
/// One `git rev-list` lists every commit the tips hold that the target does
/// not, with its parents. A tip whose commits leave that set through exactly
/// one parent has that parent as its only merge base: every commit the tip
/// shares with the target lies below it. Git reads the commit-graph, where
/// libgit2 would walk each tip's history to the target in turn. Tips that
/// leave the set through several parents, or none, ask libgit2 instead, as
/// every tip does when the listing fails.
fn merge_bases_with_target(
    repo: &Repository,
    target: Oid,
    tips: &[Oid],
) -> Result<HashMap<Oid, Oid>> {
    let mut bases = HashMap::new();
    if tips.is_empty() {
        return Ok(bases);
    }
    let mut command = crate::repository::git_command(repo);
    command.args(["rev-list", "--parents", "--stdin"]);
    let mut input = format!("^{target}\n");
    for tip in tips {
        input.push_str(&format!("{tip}\n"));
    }
    let mut parents: HashMap<Oid, Vec<Oid>> = HashMap::new();
    if let Ok(output) = output_with_stdin(command, input)
        && output.status.success()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut ids = line.split_whitespace().map(Oid::from_str);
            if let Some(Ok(commit)) = ids.next() {
                parents.insert(commit, ids.collect::<Result<_, _>>()?);
            }
        }
    }

    for &tip in tips {
        let mut exits = HashSet::new();
        if parents.contains_key(&tip) {
            let mut seen = HashSet::new();
            let mut pending = vec![tip];
            while let Some(commit) = pending.pop() {
                if !seen.insert(commit) {
                    continue;
                }
                for &parent in &parents[&commit] {
                    if parents.contains_key(&parent) {
                        pending.push(parent);
                    } else {
                        exits.insert(parent);
                    }
                }
            }
        }
        let base = match exits.len() {
            1 => exits.into_iter().next(),
            _ => repo.merge_base(tip, target).ok(),
        };
        if let Some(base) = base {
            bases.insert(tip, base);
        }
    }
    Ok(bases)
}

/// The best common ancestor of all `commits`, as `git merge-base --octopus`
/// finds it, or `None` when they share none or git cannot tell. It matches
/// libgit2's `merge_base_octopus`, but git reads the commit-graph.
fn octopus_merge_base(repo: &Repository, commits: &[Oid]) -> Result<Option<Oid>> {
    let mut seen = HashSet::new();
    let output = crate::repository::git_command(repo)
        .args(["merge-base", "--octopus"])
        .args(
            commits
                .iter()
                .filter(|commit| seen.insert(**commit))
                .map(Oid::to_string),
        )
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.lines().next().map(Oid::from_str).transpose()?)
}

pub struct FloatingTargetContext {
    candidates: Vec<FloatingTargetCandidate>,
    candidate_ids: HashSet<Oid>,
    candidate_positions: HashMap<Oid, usize>,
    patch_ids: HashSet<String>,
    /// Paths touched by each commit behind `patch_ids`, as sets. A branch
    /// commit can only share a patch id with a target commit that touches the
    /// same paths, so this prunes the patch-id fallback to the few candidates
    /// that could match. `None` when a merge commit is in the private lineage;
    /// its combined diff has no single path set to compare against.
    patch_path_sets: Option<HashSet<Vec<String>>>,
    reflog_ids: HashSet<Oid>,
    historical_tip_ids: HashSet<Oid>,
}

#[derive(Clone)]
struct FloatingTargetCandidate {
    id: Oid,
    tree_id: Oid,
    summary: String,
    email: String,
    parent_id: Option<Oid>,
}

#[derive(Clone, Copy)]
struct FloatingBaseMatch {
    branch_id: Oid,
    target_index: usize,
}

fn floating_patch_id_boundary(
    repo: &Repository,
    target_id: Oid,
    target_branch: &str,
) -> Result<Option<Oid>> {
    let upstream_name = match crate::trunk::resolve_trunk(repo)? {
        Some(name) => name,
        None => return Ok(None),
    };

    if target_branch == upstream_name {
        return Ok(None);
    }

    let upstream_id = crate::trunk::stack_base(repo, &upstream_name)?;
    let merge_base = repo.merge_base(target_id, upstream_id)?;
    let stack_branches = get_stack_branches(repo, target_id, upstream_id, &upstream_name)?;

    if !stack_branches
        .iter()
        .any(|branch| branch.name == target_branch)
    {
        return Ok(Some(merge_base));
    }

    Ok(Some(
        find_parent_in_stack(repo, target_branch, &stack_branches, merge_base)?.id(),
    ))
}

pub fn build_floating_target_context(
    repo: &Repository,
    target_commit: &Commit,
    target_branch: &str,
    history_limit: usize,
    patch_id_cache: &mut HashMap<Oid, Option<String>>,
) -> Result<FloatingTargetContext> {
    let mut all_candidates = Vec::new();
    let mut commit_ids = Vec::new();
    let mut current = Some(target_commit.id());
    let mut remaining = history_limit;

    while let Some(commit_id) = current {
        if history_limit != 0 && remaining == 0 {
            break;
        }
        let commit = repo.find_commit(commit_id)?;
        all_candidates.push(FloatingTargetCandidate {
            id: commit_id,
            tree_id: commit.tree_id(),
            summary: commit.summary().unwrap_or("").trim().to_string(),
            email: commit.author().email().unwrap_or("").to_string(),
            parent_id: if commit.parent_count() > 0 {
                Some(commit.parent_id(0)?)
            } else {
                None
            },
        });
        commit_ids.push(commit_id);
        current = if commit.parent_count() > 0 {
            Some(commit.parent_id(0)?)
        } else {
            None
        };
        if history_limit != 0 {
            remaining -= 1;
        }
    }

    let patch_id_boundary = floating_patch_id_boundary(repo, target_commit.id(), target_branch)?;
    let private_len = match patch_id_boundary {
        Some(boundary) => commit_ids
            .iter()
            .position(|oid| *oid == boundary)
            .unwrap_or(commit_ids.len()),
        None => commit_ids.len(),
    };
    let patch_commit_ids: Vec<Oid> = match patch_id_boundary {
        Some(boundary) => commit_ids
            .iter()
            .copied()
            .take_while(|oid| *oid != boundary)
            .collect(),
        None => commit_ids.clone(),
    };

    // Patch-id fallback should only compare against the target branch's private lineage.
    // Matching against upstream commits causes unrelated branches with cherry-picked
    // equivalents to look like floating children.
    ensure_patch_ids(repo, &patch_commit_ids, patch_id_cache)?;
    let patch_ids = patch_commit_ids
        .iter()
        .filter_map(|oid| patch_id_cache.get(oid).and_then(|v| v.as_ref()).cloned())
        .collect();
    let patch_path_sets = if patch_commit_ids.is_empty() {
        Some(HashSet::new())
    } else if patch_commit_ids
        .iter()
        .map(|oid| {
            repo.find_commit(*oid)
                .map(|commit| commit.parent_count() > 1)
        })
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .any(|is_merge| is_merge)
    {
        None
    } else {
        Some(
            touched_path_sets(repo, &patch_commit_ids)?
                .into_values()
                .collect(),
        )
    };
    let candidates: Vec<FloatingTargetCandidate> =
        all_candidates.into_iter().take(private_len).collect();
    let candidate_ids = candidates.iter().map(|candidate| candidate.id).collect();
    let candidate_positions = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| (candidate.id, index))
        .collect();
    let reflog_ids = read_branch_reflog_ids(repo, target_branch);

    // Exact shared ancestors need evidence that they belonged to this branch,
    // and a private boundary so shared upstream history cannot adopt siblings.
    let mut historical_tip_ids = HashSet::new();
    if patch_id_boundary.is_some() {
        historical_tip_ids.extend(reflog_ids.iter().copied());
        if let Ok(branch) = repo.find_branch(target_branch, git2::BranchType::Local)
            && let Ok(upstream) = branch.upstream()
            && upstream.get().is_remote()
            && let Some(tip) = upstream.get().target()
        {
            historical_tip_ids.insert(tip);
        }
    }

    Ok(FloatingTargetContext {
        candidates,
        candidate_ids,
        candidate_positions,
        patch_ids,
        patch_path_sets,
        reflog_ids,
        historical_tip_ids,
    })
}

/// For each branch tip, the commit on its history that the target branch was
/// rewritten from (its floating base), or `None` when the branch is not a
/// floating child of the target.
///
/// The tips are resolved together because the patch-id fallback needs a
/// `git show | git patch-id` round trip per batch of commits; sharing it lets
/// every branch's candidates go out in one batch instead of one pair of
/// processes per branch.
pub fn find_floating_bases(
    repo: &Repository,
    branch_tips: &[Oid],
    target: &FloatingTargetContext,
    history_limit: usize,
    patch_id_cache: &mut HashMap<Oid, Option<String>>,
) -> Result<Vec<Option<Oid>>> {
    let mut walks = Vec::with_capacity(branch_tips.len());
    let mut pending = Vec::new();
    for &branch_tip in branch_tips {
        let walk =
            walk_floating_candidates(repo, branch_tip, target, history_limit, patch_id_cache)?;
        if let FloatingWalk::PatchCandidates(candidates) = &walk {
            pending.extend(candidates.iter().copied());
        }
        walks.push(walk);
    }
    let pending = filter_patch_candidates(repo, target, &pending)?;
    ensure_patch_ids(repo, &pending, patch_id_cache)?;
    let mut results = Vec::with_capacity(branch_tips.len());
    for (walk, &branch_tip) in walks.into_iter().zip(branch_tips) {
        let found = match walk {
            FloatingWalk::Resolved(found) => found,
            FloatingWalk::PatchCandidates(candidates) => {
                match_floating_patch_ids(repo, branch_tip, target, &candidates, patch_id_cache)?
            }
        };
        results.push(found.map(|matching| matching.branch_id));
    }
    Ok(results)
}

/// Outcome of walking a branch's first-parent history against the target.
enum FloatingWalk {
    /// The walk settled the answer on its own, by an exact, tree, or metadata match.
    Resolved(Option<FloatingBaseMatch>),
    /// The walk found nothing conclusive; these private commits remain to be
    /// compared by patch id (see [`match_floating_patch_ids`]).
    PatchCandidates(Vec<Oid>),
}

/// The merge bases of a branch tip and the target, or none when the histories
/// are unrelated. Every reachability question the walk asks reduces to these:
/// the target is an ancestor of the tip exactly when it is one of the bases,
/// and a commit on the tip's history is reachable from the target exactly when
/// some base reaches it. Resolving them once per branch keeps each question a
/// short walk near the fork point rather than a paint-down from the target
/// over everything the branch has fallen behind.
fn floating_merge_bases(repo: &Repository, branch_tip: Oid, target_id: Oid) -> Result<Vec<Oid>> {
    match repo.merge_bases(branch_tip, target_id) {
        Ok(bases) => Ok(bases.iter().copied().collect()),
        Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(Vec::new()),
        Err(err) => Err(err.into()),
    }
}

/// Keep only the patch candidates that could share a patch id with a target
/// commit: those touching exactly the paths some target commit touches. Merge
/// commits are kept as they are; they have no single path set to compare.
/// Candidates are deduplicated, preserving first occurrence.
fn filter_patch_candidates(
    repo: &Repository,
    target: &FloatingTargetContext,
    candidates: &[Oid],
) -> Result<Vec<Oid>> {
    let mut seen = HashSet::new();
    let candidates: Vec<Oid> = candidates
        .iter()
        .copied()
        .filter(|oid| seen.insert(*oid))
        .collect();
    let Some(target_path_sets) = &target.patch_path_sets else {
        return Ok(candidates);
    };
    let mut kept = Vec::new();
    let mut comparable = Vec::new();
    for oid in candidates {
        if repo.find_commit(oid)?.parent_count() > 1 {
            kept.push(oid);
        } else {
            comparable.push(oid);
        }
    }
    let path_sets = touched_path_sets(repo, &comparable)?;
    for oid in comparable {
        let paths = path_sets.get(&oid).cloned().unwrap_or_default();
        if target_path_sets.contains(&paths) {
            kept.push(oid);
        }
    }
    Ok(kept)
}

/// The sorted set of paths each commit changes against its first parent (or
/// against the empty tree for a root commit), in one `git diff-tree` run.
///
/// The output is read as NUL-delimited `--name-status` records: a bare commit
/// id opens a commit's block, and every change follows as a status record and
/// then its path record. A path is always consumed as the record after a
/// status, so a file whose name happens to spell a commit id cannot be taken
/// for a header.
fn touched_path_sets(repo: &Repository, commit_ids: &[Oid]) -> Result<HashMap<Oid, Vec<String>>> {
    let mut sets: HashMap<Oid, Vec<String>> = HashMap::new();
    if commit_ids.is_empty() {
        return Ok(sets);
    }
    let mut command = crate::repository::git_command(repo);
    command
        .arg("diff-tree")
        .arg("-r")
        .arg("--root")
        .arg("--name-status")
        .arg("-z")
        .arg("--no-ext-diff")
        .arg("--stdin")
        .current_dir(repo_root(repo)?);
    let input: String = commit_ids.iter().map(|oid| format!("{oid}\n")).collect();
    let output = output_with_stdin(command, input)?;
    if !output.status.success() {
        return Err(anyhow!("git diff-tree failed while listing touched paths."));
    }
    let requested: HashSet<Oid> = commit_ids.iter().copied().collect();
    let mut current: Option<Oid> = None;
    let mut records = output.stdout.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        let record = String::from_utf8_lossy(record);
        if let Ok(oid) = Oid::from_str(&record)
            && requested.contains(&oid)
        {
            sets.entry(oid).or_default();
            current = Some(oid);
            continue;
        }
        // Any other record is a change status; the path is the record after it.
        let Some(path) = records.next() else {
            break;
        };
        if let Some(oid) = current {
            sets.entry(oid)
                .or_default()
                .push(String::from_utf8_lossy(path).into_owned());
        }
    }
    for paths in sets.values_mut() {
        paths.sort();
        paths.dedup();
    }
    Ok(sets)
}

fn reachable_from_merge_bases(repo: &Repository, bases: &[Oid], oid: Oid) -> Result<bool> {
    if bases.contains(&oid) {
        return Ok(true);
    }
    for &base in bases {
        if repo.graph_descendant_of(base, oid)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn walk_floating_candidates(
    repo: &Repository,
    branch_tip: Oid,
    target: &FloatingTargetContext,
    history_limit: usize,
    patch_id_cache: &mut HashMap<Oid, Option<String>>,
) -> Result<FloatingWalk> {
    let Some(target_id) = target.candidates.first().map(|candidate| candidate.id) else {
        return Ok(FloatingWalk::Resolved(None));
    };

    // If the branch is already on top of, already part of, or already integrated
    // into the target branch, it is not a floating child that needs restacking.
    if branch_tip == target_id {
        return Ok(FloatingWalk::Resolved(None));
    }
    let merge_bases = floating_merge_bases(repo, branch_tip, target_id)?;
    if merge_bases.contains(&target_id) || merge_bases.contains(&branch_tip) {
        return Ok(FloatingWalk::Resolved(None));
    }

    let mut patch_candidates = Vec::new();
    let mut current = Some(branch_tip);
    let mut remaining = history_limit;

    while let Some(oid) = current {
        if history_limit != 0 && remaining == 0 {
            break;
        }

        // An unchanged commit in the target's private lineage is an exact fork
        // point when the parent gained commits. Check before stopping at shared
        // history; upstream commits are excluded from candidate_positions.
        if oid != branch_tip
            && target.historical_tip_ids.contains(&oid)
            && let Some(&target_index) = target.candidate_positions.get(&oid)
        {
            return Ok(FloatingWalk::Resolved(Some(FloatingBaseMatch {
                branch_id: oid,
                target_index,
            })));
        }

        // Optimization: If we hit a commit that is reachable from the target, we stop.
        // Because any match found *after* this point would be a common ancestor, not a floating base.
        if reachable_from_merge_bases(repo, &merge_bases, oid)? {
            break;
        }

        let commit = repo.find_commit(oid)?;
        let parent_id = first_parent_id(&commit)?;
        current = parent_id;
        if history_limit != 0 {
            remaining -= 1;
        }

        if let Some(&target_index) = target.candidate_positions.get(&oid) {
            if oid != branch_tip {
                let matching = FloatingBaseMatch {
                    branch_id: oid,
                    target_index,
                };
                if validate_floating_match(repo, branch_tip, matching, target, patch_id_cache)? {
                    return Ok(FloatingWalk::Resolved(Some(matching)));
                }
            }
            continue;
        }

        // Match by tree-hash against rewritten target commits, but only if the
        // surrounding parent commit also lines up with the rewritten lineage.
        for (target_index, candidate) in target.candidates.iter().enumerate() {
            if candidate.tree_id != commit.tree_id() {
                continue;
            }
            if !floating_parent_matches_candidate(
                repo,
                parent_id,
                candidate.parent_id,
                patch_id_cache,
            )? {
                continue;
            }
            if oid != branch_tip {
                let matching = FloatingBaseMatch {
                    branch_id: oid,
                    target_index,
                };
                if validate_floating_match(repo, branch_tip, matching, target, patch_id_cache)? {
                    return Ok(FloatingWalk::Resolved(Some(matching)));
                }
            }
        }

        // Metadata matches narrow the patch-id fallback, but are not sufficient on their own.
        if let Some(target_index) = metadata_matches_target_candidate(&commit, oid, target)?
            && oid != branch_tip
        {
            let matching = FloatingBaseMatch {
                branch_id: oid,
                target_index,
            };
            if validate_floating_match(repo, branch_tip, matching, target, patch_id_cache)? {
                return Ok(FloatingWalk::Resolved(Some(matching)));
            }
            continue;
        }

        // Check if metadata matches a target commit but trees differ.
        // This indicates the target was modified (e.g., fixup during rebase)
        // and this commit is the original version - fork point found.
        // This handles the case where:
        // - cli-tree has: main -> old_pty -> old_cli
        // - pty-alive was rebased to: main -> new_pty (modified) -> ...
        // - old_pty and new_pty have same metadata but different trees
        // - old_pty is NOT in candidate_ids (it's the old version)
        //
        // BUT we must verify there's an actual rebase relationship.
        // In the embers case, old_pty was rebased to create new_pty, so:
        //   - old_pty.parent IS in pty-alive's history
        //   - new_pty.parent (which is old_pty) is NOT in pty-alive's history
        //
        // In the sibling case (old_base and rewritten_main):
        //   - Both share the same parent (shared_parent) which IS in main's history
        //   - Neither is a rebased version of the other
        //
        // So we check: if the corresponding target commit's parent is in target history,
        // they are siblings, not a rebase pair.
        if oid != branch_tip {
            let summary = commit.summary().unwrap_or("").trim().to_string();
            let author = commit.author();
            let email = author.email().unwrap_or("").to_string();

            // Look for a target commit with matching summary, author, email but DIFFERENT tree.
            // We don't require parent_id to match because after a rebase, the parent changes.
            // We check parent ancestry to distinguish rebase from sibling:
            // - Rebase: OLD.parent is NOT an ancestor of ANY candidate in the target chain
            //   (because OLD.parent was rebased and is now "orphaned")
            // - Sibling: OLD.parent IS an ancestor of some candidate in target chain
            //   (because OLD.parent is on the main lineage which target is built on)
            // Tree-mismatch matching is only reliable against the current target tip.
            // Matching lower-history commits by summary/email is too ambiguous and can
            // falsely classify unrelated side branches as floating.
            let corresponding_target = target.candidates.iter().find(|c| {
                c.id == target_id
                    && c.summary == summary
                    && c.email == email
                    && c.tree_id != commit.tree_id()
            });

            if let Some(_target_candidate) = corresponding_target {
                // Check if OLD.parent is an ancestor of ANY candidate in target chain.
                // In rebase: old_pty_1 is not an ancestor of any new pty candidate (orphaned).
                // In sibling: A is an ancestor of main' (through main), so it IS an ancestor of candidates.
                let old_parent_ancestor_of_any_candidate = parent_id
                    .map(|pid| {
                        target
                            .candidate_ids
                            .iter()
                            .any(|cid| repo.graph_descendant_of(*cid, pid).unwrap_or(false))
                    })
                    .unwrap_or(false);

                if !old_parent_ancestor_of_any_candidate {
                    // OLD.parent is NOT an ancestor of any candidate - it's orphaned, rebase fork
                    let matching = FloatingBaseMatch {
                        branch_id: oid,
                        target_index: 0,
                    };
                    if validate_floating_match(repo, branch_tip, matching, target, patch_id_cache)?
                    {
                        return Ok(FloatingWalk::Resolved(Some(matching)));
                    }
                }
            }
        }

        patch_candidates.push(oid);
    }

    // No private target commit means no patch id can match; skip the round trip.
    if target.patch_ids.is_empty() {
        return Ok(FloatingWalk::Resolved(None));
    }
    Ok(FloatingWalk::PatchCandidates(patch_candidates))
}

/// Patch-id fallback of the floating walk: match the branch's private commits
/// against rewritten target commits by content. Callers must have populated
/// `patch_id_cache` for `patch_candidates` (see [`ensure_patch_ids`]).
fn match_floating_patch_ids(
    repo: &Repository,
    branch_tip: Oid,
    target: &FloatingTargetContext,
    patch_candidates: &[Oid],
    patch_id_cache: &mut HashMap<Oid, Option<String>>,
) -> Result<Option<FloatingBaseMatch>> {
    let Some(target_id) = target.candidates.first().map(|candidate| candidate.id) else {
        return Ok(None);
    };
    for &oid in patch_candidates {
        if oid == branch_tip {
            continue;
        }
        let Some(patch_id) = patch_id_cache.get(&oid).and_then(|v| v.as_ref()).cloned() else {
            continue;
        };
        if !target.patch_ids.contains(&patch_id) {
            continue;
        }

        let commit = repo.find_commit(oid)?;
        let parent_id = first_parent_id(&commit)?;
        for (target_index, candidate) in target.candidates.iter().enumerate() {
            let Some(candidate_patch_id) =
                patch_id_cache.get(&candidate.id).and_then(|v| v.as_ref())
            else {
                continue;
            };
            if candidate_patch_id != &patch_id {
                continue;
            }
            let parent_matches = floating_parent_matches_candidate(
                repo,
                parent_id,
                candidate.parent_id,
                patch_id_cache,
            )?;
            if !(parent_matches || is_tip_patch_rewrite(&commit, candidate, target_id)) {
                continue;
            }

            let matching = FloatingBaseMatch {
                branch_id: oid,
                target_index,
            };
            if validate_floating_match(repo, branch_tip, matching, target, patch_id_cache)? {
                return Ok(Some(matching));
            }
        }
    }

    Ok(None)
}

/// Returns the first parent's OID if the commit has at least one parent.
///
/// Returns `Ok(None)` for root commits (commits with no parents).
/// Propagates errors from `parent_id()` via the `?` operator.
fn first_parent_id(commit: &Commit) -> Result<Option<Oid>> {
    if commit.parent_count() > 0 {
        Ok(Some(commit.parent_id(0)?))
    } else {
        Ok(None)
    }
}

/// Checks whether the floating branch's parent commit aligns with a target
/// candidate's parent.
///
/// Returns `true` if parents match by OID, tree-id, or patch-id—indicating
/// the commits share the same logical lineage despite potential rewrites.
///
/// Comparison strategy:
/// 1. OID equality (fastest)
/// 2. Tree-ID equality (fallback when commit was amended but patch is intact)
/// 3. Patch-ID equality via `patch_id_cache` (handles content-preserving rewrites)
///
/// When both `branch_parent_id` and `candidate_parent_id` are `None` (both
/// root commits), returns `true`.
///
/// Calls `ensure_patch_ids` to populate the cache before patch comparison.
/// Returns `Result<bool>` due to repository lookup errors (`find_commit`).
fn floating_parent_matches_candidate(
    repo: &Repository,
    branch_parent_id: Option<Oid>,
    candidate_parent_id: Option<Oid>,
    patch_id_cache: &mut HashMap<Oid, Option<String>>,
) -> Result<bool> {
    let (Some(branch_parent_id), Some(candidate_parent_id)) =
        (branch_parent_id, candidate_parent_id)
    else {
        return Ok(branch_parent_id.is_none() && candidate_parent_id.is_none());
    };

    if branch_parent_id == candidate_parent_id {
        return Ok(true);
    }

    if repo.find_commit(branch_parent_id)?.tree_id()
        == repo.find_commit(candidate_parent_id)?.tree_id()
    {
        return Ok(true);
    }

    ensure_patch_ids(
        repo,
        &[branch_parent_id, candidate_parent_id],
        patch_id_cache,
    )?;
    let branch_patch_id = patch_id_cache
        .get(&branch_parent_id)
        .and_then(|value| value.as_ref());
    let candidate_patch_id = patch_id_cache
        .get(&candidate_parent_id)
        .and_then(|value| value.as_ref());

    Ok(branch_patch_id.is_some() && branch_patch_id == candidate_patch_id)
}

/// Detects the special case where a floating commit's patch-id matches the
/// target tip but the trees differ—indicating the target tip was modified
/// (e.g., via fixup/squash) and the floating commit is the original version.
///
/// Requires matching commit summary and author email for safety to avoid
/// false positives when different commits happen to have the same patch-id.
///
/// Returns `true` when all conditions hold:
/// - `candidate.id == target_id` (same patch-id as target)
/// - `candidate.tree_id != commit.tree_id()` (trees differ)
/// - `candidate.summary == commit.summary().unwrap_or("").trim()` (summary matches)
/// - `candidate.email == commit.author().email().unwrap_or("")` (email matches)
///
/// Returns a simple `bool` (no error propagation needed).
fn is_tip_patch_rewrite(
    commit: &Commit,
    candidate: &FloatingTargetCandidate,
    target_id: Oid,
) -> bool {
    candidate.id == target_id
        && candidate.tree_id != commit.tree_id()
        && candidate.summary == commit.summary().unwrap_or("").trim()
        && candidate.email == commit.author().email().unwrap_or("")
}

fn metadata_matches_target_candidate(
    commit: &Commit,
    oid: Oid,
    target: &FloatingTargetContext,
) -> Result<Option<usize>> {
    if !target.reflog_ids.contains(&oid) {
        return Ok(None);
    }

    let summary = commit.summary().unwrap_or("").trim();
    let author = commit.author();
    let email = author.email().unwrap_or("");
    let parent_id = if commit.parent_count() > 0 {
        Some(commit.parent_id(0)?)
    } else {
        None
    };

    Ok(target.candidates.iter().position(|candidate| {
        candidate.summary == summary && candidate.email == email && candidate.parent_id == parent_id
    }))
}

fn validate_floating_match(
    repo: &Repository,
    branch_tip: Oid,
    matching: FloatingBaseMatch,
    target: &FloatingTargetContext,
    patch_id_cache: &mut HashMap<Oid, Option<String>>,
) -> Result<bool> {
    let Some(target_tip) = target.candidates.first().map(|candidate| candidate.id) else {
        return Ok(false);
    };
    let branch_chain = match repo.merge_base(branch_tip, target_tip) {
        Ok(graph_merge_base) => collect_first_parent_chain(repo, graph_merge_base, branch_tip)?,
        Err(_) => collect_first_parent_chain_to_root(repo, branch_tip)?,
    };
    let Some(match_index) = branch_chain
        .iter()
        .position(|oid| *oid == matching.branch_id)
    else {
        return Ok(false);
    };

    // Floating branches must have at least one commit ahead of the matched old base.
    if match_index + 1 >= branch_chain.len() {
        return Ok(false);
    }

    let mut chain_len = 1usize;
    let mut branch_index = match_index;
    let mut target_index = matching.target_index;

    while branch_index > 0 && target_index + 1 < target.candidates.len() {
        let branch_oid = branch_chain[branch_index - 1];
        let candidate = &target.candidates[target_index + 1];
        if !branch_commit_matches_target_candidate(
            repo,
            branch_oid,
            candidate,
            target,
            patch_id_cache,
        )? {
            break;
        }
        chain_len += 1;
        branch_index -= 1;
        target_index += 1;
    }

    if chain_len >= 2 {
        return Ok(true);
    }

    // Fallback: a single matched point is enough only when it is the branch's immediate
    // fork point above the graph merge-base, meaning all newer commits belong to the suffix
    // that should be replayed.
    Ok(match_index == 0)
}

fn collect_first_parent_chain_to_root(repo: &Repository, tip: Oid) -> Result<Vec<Oid>> {
    let mut chain = Vec::new();
    let mut current = Some(tip);

    while let Some(oid) = current {
        chain.push(oid);
        let commit = repo.find_commit(oid)?;
        current = if commit.parent_count() > 0 {
            Some(commit.parent_id(0)?)
        } else {
            None
        };
    }

    chain.reverse();
    Ok(chain)
}

fn branch_commit_matches_target_candidate(
    repo: &Repository,
    branch_oid: Oid,
    candidate: &FloatingTargetCandidate,
    target: &FloatingTargetContext,
    patch_id_cache: &mut HashMap<Oid, Option<String>>,
) -> Result<bool> {
    if branch_oid == candidate.id {
        return Ok(true);
    }

    let branch_commit = repo.find_commit(branch_oid)?;
    if branch_commit.tree_id() == candidate.tree_id {
        return Ok(true);
    }

    ensure_patch_ids(repo, &[branch_oid, candidate.id], patch_id_cache)?;
    let branch_patch_id = patch_id_cache
        .get(&branch_oid)
        .and_then(|value| value.as_ref());
    let candidate_patch_id = patch_id_cache
        .get(&candidate.id)
        .and_then(|value| value.as_ref());
    if branch_patch_id.is_some() && branch_patch_id == candidate_patch_id {
        return Ok(true);
    }

    if target.reflog_ids.contains(&branch_oid)
        && candidate.summary == branch_commit.summary().unwrap_or("").trim()
        && candidate.email == branch_commit.author().email().unwrap_or("")
    {
        return Ok(true);
    }

    Ok(false)
}

fn read_branch_reflog_ids(repo: &Repository, branch_name: &str) -> HashSet<Oid> {
    let reflog_name = if branch_name.starts_with("refs/") {
        branch_name.to_string()
    } else {
        format!("refs/heads/{branch_name}")
    };

    let Ok(reflog) = repo.reflog(&reflog_name) else {
        return HashSet::new();
    };

    let mut ids = HashSet::new();
    for index in 0..reflog.len() {
        if let Some(entry) = reflog.get(index) {
            let oid = entry.id_new();
            if !oid.is_zero() {
                ids.insert(oid);
            }
            let oid = entry.id_old();
            if !oid.is_zero() {
                ids.insert(oid);
            }
        }
    }

    ids
}

fn ensure_patch_ids(
    repo: &Repository,
    commit_ids: &[Oid],
    patch_id_cache: &mut HashMap<Oid, Option<String>>,
) -> Result<()> {
    let missing: Vec<Oid> = commit_ids
        .iter()
        .copied()
        .filter(|oid| !patch_id_cache.contains_key(oid))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }

    let computed = compute_patch_ids(repo, &missing)?;
    for oid in missing {
        patch_id_cache.insert(oid, computed.get(&oid).cloned());
    }

    Ok(())
}

fn compute_patch_ids(repo: &Repository, commit_ids: &[Oid]) -> Result<HashMap<Oid, String>> {
    if commit_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let repo_root = repo_root(repo)?;
    let mut result = HashMap::new();
    const PATCH_ID_BATCH_SIZE: usize = 512;

    for chunk in commit_ids.chunks(PATCH_ID_BATCH_SIZE) {
        let mut show = crate::repository::git_command(repo);
        show.arg("show").arg("--no-ext-diff").arg("--no-color");
        for oid in chunk {
            show.arg(oid.to_string());
        }

        let mut show_child = show.current_dir(repo_root).stdout(Stdio::piped()).spawn()?;

        let show_stdout = show_child.stdout.take().ok_or_else(|| {
            anyhow!("Failed to capture git show output for patch-id calculation.")
        })?;

        let patch_output = crate::repository::git_command(repo)
            .arg("patch-id")
            .arg("--stable")
            .current_dir(repo_root)
            .stdin(Stdio::from(show_stdout))
            .output()?;

        let show_status = show_child.wait()?;
        if !show_status.success() {
            return Err(anyhow!("git show failed while computing patch ids."));
        }
        if !patch_output.status.success() {
            return Err(anyhow!("git patch-id failed while computing patch ids."));
        }

        let stdout = String::from_utf8_lossy(&patch_output.stdout);
        for line in stdout.lines() {
            let mut parts = line.split_whitespace();
            let patch_id = parts.next().unwrap_or_default();
            let commit_id = parts.next().unwrap_or_default();
            if patch_id.is_empty() || commit_id.is_empty() {
                continue;
            }
            if let Ok(oid) = Oid::from_str(commit_id) {
                result.insert(oid, patch_id.to_string());
            }
        }
    }

    Ok(result)
}

fn range_changes_present_in_target(
    repo: &Repository,
    base_id: Oid,
    branch_tip: Oid,
    target_tip: Oid,
    cache: &mut SyncScanCache,
) -> Result<bool> {
    let touched_paths = cache.touched_paths(repo, base_id, branch_tip)?;

    if let Some(present) =
        range_changes_settled_by_trees(repo, base_id, branch_tip, target_tip, &touched_paths)?
    {
        return Ok(present);
    }

    let target_history = cache.history(repo, base_id, target_tip, &touched_paths)?;
    range_changes_present_in_history(repo, base_id, branch_tip, &touched_paths, &target_history)
}

fn range_touched_paths(repo: &Repository, base_id: Oid, branch_tip: Oid) -> Result<Vec<String>> {
    if branch_tip == base_id {
        return Ok(Vec::new());
    }

    let base_tree = repo.find_commit(base_id)?.tree()?;
    let tip_tree = repo.find_commit(branch_tip)?.tree()?;
    let mut opts = git2::DiffOptions::new();
    opts.include_typechange(true);
    let diff = repo.diff_tree_to_tree(Some(&base_tree), Some(&tip_tree), Some(&mut opts))?;

    // The paths are looked up in trees verbatim, so they must be the real names:
    // never lossily converted, and never quoted the way `git diff` prints them.
    let mut paths = Vec::new();
    for delta in diff.deltas() {
        let Some(path) = delta.new_file().path().or_else(|| delta.old_file().path()) else {
            continue;
        };
        let path = path
            .to_str()
            .ok_or_else(|| anyhow!("Path {} is not valid UTF-8.", path.display()))?;
        paths.push(path.to_string());
    }
    Ok(paths)
}

/// Answers containment from the touched paths' contents alone, when the target
/// either matches the branch tip or still matches the base. `None` means the
/// target history has to be searched for the branch's patch.
fn range_changes_settled_by_trees(
    repo: &Repository,
    base_id: Oid,
    branch_tip: Oid,
    target_tip: Oid,
    touched_paths: &[String],
) -> Result<Option<bool>> {
    if touched_paths.is_empty() {
        return Ok(Some(true));
    }

    if commits_match_on_paths(repo, branch_tip, target_tip, touched_paths)? {
        return Ok(Some(true));
    }

    if commits_match_on_paths(repo, base_id, target_tip, touched_paths)? {
        return Ok(Some(false));
    }

    Ok(None)
}

fn range_changes_present_in_history(
    repo: &Repository,
    base_id: Oid,
    branch_tip: Oid,
    touched_paths: &[String],
    target_history: &TargetPathHistory,
) -> Result<bool> {
    let candidate_commits = target_history.covering_commits(repo, base_id, touched_paths)?;
    if candidate_commits.is_empty() {
        return Ok(false);
    }

    let base_tree = repo.find_commit(base_id)?.tree()?;
    let tip_tree = repo.find_commit(branch_tip)?.tree()?;
    let branch_patch_id = paths_patch_id(repo, Some(&base_tree), &tip_tree, touched_paths)?
        .ok_or_else(|| anyhow!("Missing branch patch id while checking target containment."))?;
    let target_patch_ids = commit_patch_ids_for_paths(repo, &candidate_commits, touched_paths)?;

    Ok(target_patch_ids.contains(&branch_patch_id))
}

fn commits_match_on_paths(
    repo: &Repository,
    left_tip: Oid,
    right_tip: Oid,
    touched_paths: &[String],
) -> Result<bool> {
    let left_tree = repo.find_commit(left_tip)?.tree()?;
    let right_tree = repo.find_commit(right_tip)?.tree()?;

    for path in touched_paths {
        if tree_entry_state(&left_tree, Path::new(path))?
            != tree_entry_state(&right_tree, Path::new(path))?
        {
            return Ok(false);
        }
    }

    Ok(true)
}

fn tree_entry_state(tree: &git2::Tree<'_>, path: &Path) -> Result<Option<(Oid, i32)>> {
    match tree.get_path(path) {
        Ok(entry) => Ok(Some((entry.id(), entry.filemode()))),
        Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Patch id of the change from `old` to `new` on `paths`, or `None` when the
/// paths do not change. Hashed without context, so a change still matches
/// after upstream has moved the lines around it; both sides of a comparison
/// must come from here for the ids to be comparable.
fn paths_patch_id(
    repo: &Repository,
    old: Option<&git2::Tree<'_>>,
    new: &git2::Tree<'_>,
    paths: &[String],
) -> Result<Option<Oid>> {
    let mut opts = git2::DiffOptions::new();
    opts.context_lines(0)
        .include_typechange(true)
        .disable_pathspec_match(true);
    for path in paths {
        opts.pathspec(path);
    }
    let diff = repo.diff_tree_to_tree(old, Some(new), Some(&mut opts))?;
    if diff.deltas().len() == 0 {
        return Ok(None);
    }
    Ok(Some(diff.patchid(None)?))
}

/// Patch ids of the non-merge `commit_ids` against their parent, on `paths`.
/// A merge commit has no single change to compare, so it is skipped.
fn commit_patch_ids_for_paths(
    repo: &Repository,
    commit_ids: &[Oid],
    paths: &[String],
) -> Result<Vec<Oid>> {
    let mut patch_ids = Vec::new();
    for &commit_id in commit_ids {
        let commit = repo.find_commit(commit_id)?;
        let parent_tree = match commit.parent_count() {
            0 => None,
            1 => Some(commit.parent(0)?.tree()?),
            _ => continue,
        };
        if let Some(patch_id) = paths_patch_id(repo, parent_tree.as_ref(), &commit.tree()?, paths)?
        {
            patch_ids.push(patch_id);
        }
    }
    Ok(patch_ids)
}

fn ordered_stack_lineage(
    repo: &Repository,
    top_id: Oid,
    stack_branches: &[StackBranch],
) -> Result<Vec<StackBranch>> {
    let mut lineage = Vec::new();
    for branch in stack_branches {
        if branch.id == top_id || repo.graph_descendant_of(top_id, branch.id)? {
            lineage.push(branch.clone());
        }
    }

    sort_branches_topologically(repo, &mut lineage)?;
    Ok(lineage)
}

fn branch_segment_integrated(
    repo: &Repository,
    old_base: Oid,
    branch_tip: Oid,
    upstream_id: Oid,
    cache: &mut SyncScanCache,
) -> Result<bool> {
    if repo.graph_descendant_of(upstream_id, branch_tip)? {
        return Ok(true);
    }

    range_changes_present_in_target(repo, old_base, branch_tip, upstream_id, cache)
}

fn repo_root(repo: &Repository) -> Result<&Path> {
    if let Some(workdir) = repo.workdir() {
        return Ok(workdir);
    }

    repo.path()
        .parent()
        .ok_or_else(|| anyhow!("Failed to resolve repository root path."))
}

pub fn resolve_merge_base(repo: &Repository, a: Oid, b: Oid) -> Result<Oid> {
    match repo.merge_base(a, b) {
        Ok(merge_base) => Ok(merge_base),
        Err(err) if err.code() == git2::ErrorCode::NotFound => git_merge_base(repo, a, b),
        Err(err) => Err(err.into()),
    }
}

fn git_merge_base(repo: &Repository, a: Oid, b: Oid) -> Result<Oid> {
    let output = crate::repository::git_command(repo)
        .arg("merge-base")
        .arg(a.to_string())
        .arg(b.to_string())
        .current_dir(repo_root(repo)?)
        .output()?;

    if !output.status.success() {
        return Err(anyhow!("no merge base found between {} and {}.", a, b));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let merge_base = stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| anyhow!("git merge-base produced no output."))?;
    Oid::from_str(merge_base.trim()).map_err(|err| err.into())
}

pub fn get_stack_branches(
    repo: &Repository,
    head_id: Oid,
    upstream_id: Oid,
    upstream_name: &str,
) -> Result<Vec<StackBranch>> {
    let mut branches = Vec::new();
    let local_branches = repo.branches(Some(git2::BranchType::Local))?;

    // Find the merge base of HEAD and upstream.
    // Any branch that is a descendant of this merge base and NOT on upstream is part of the stack.
    let merge_base = repo.merge_base(head_id, upstream_id)?;
    let unmerged_above_merge_base = local_branches_by_ancestry(
        repo,
        &[("--contains", merge_base), ("--no-merged", upstream_id)],
    )?;
    let lineage = HeadLineage::new(repo, head_id, upstream_id)?;

    for res in local_branches {
        let (branch, _) = res?;
        let name = branch
            .name()?
            .ok_or_else(|| anyhow!("Invalid branch name"))?;
        let id = branch
            .get()
            .target()
            .ok_or_else(|| anyhow!("Branch target not found"))?;

        if name == upstream_name {
            continue;
        }

        if unmerged_above_merge_base.contains(name) && lineage.contains(name, id) {
            branches.push(StackBranch {
                name: name.to_string(),
                id,
            });
        }
    }

    Ok(branches)
}

/// The local branches on HEAD's lineage that upstream does not contain yet:
/// those whose tips are in HEAD's private history, and those whose tips
/// contain HEAD. Each part is answered for every branch at once, so the cost
/// follows the private history and not the number of local branches times
/// the depth of the history they fork from.
struct HeadLineage {
    /// Commits reachable from HEAD but not from upstream. Empty when upstream
    /// already contains HEAD.
    private: HashSet<Oid>,
    /// Branches whose tips contain HEAD and are not in upstream.
    above_head: HashSet<String>,
}

impl HeadLineage {
    fn new(repo: &Repository, head_id: Oid, upstream_id: Oid) -> Result<Self> {
        // Walk from HEAD backward, stopping at upstream. Cost is O(stack_depth),
        // not O(full repo history). TOPOLOGICAL sort avoids the timestamp-ordering
        // pitfall where libgit2 would otherwise eagerly process upstream's recent
        // commits before the (potentially older) stack commits.
        let mut private = HashSet::new();
        let mut walk = repo.revwalk()?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL)?;
        walk.push(head_id)?;
        walk.hide(upstream_id)?;
        for id in walk {
            private.insert(id?);
        }
        let above_head = local_branches_by_ancestry(
            repo,
            &[("--contains", head_id), ("--no-merged", upstream_id)],
        )?;
        Ok(Self {
            private,
            above_head,
        })
    }

    /// Whether upstream already contains HEAD, leaving it no private history.
    fn head_is_on_upstream(&self) -> bool {
        self.private.is_empty()
    }

    fn is_private(&self, id: Oid) -> bool {
        self.private.contains(&id)
    }

    fn is_above_head(&self, name: &str) -> bool {
        self.above_head.contains(name)
    }

    fn contains(&self, name: &str, id: Oid) -> bool {
        self.is_private(id) || self.is_above_head(name)
    }
}

/// Enumerates stack commits for interactive selection.
///
/// Final ordering is newest branch first, and within each branch commits are tip first.
/// Commits that appear in multiple branches are emitted once via a global `seen` set, except
/// each branch tip is always included so branches that share a head still appear in the picker.
pub fn enumerate_stack_commits(
    repo: &Repository,
    stack_branches: &[StackBranch],
    upstream_name: &str,
) -> Result<Vec<StackCommit>> {
    let mut ordered_branches = stack_branches.to_vec();
    sort_branches_topologically(repo, &mut ordered_branches)?;

    let upstream_id = crate::trunk::stack_base(repo, upstream_name)?;
    let mut seen = HashSet::new();
    let mut branch_chunks = Vec::new();

    for branch in ordered_branches {
        let merge_base = repo.merge_base(upstream_id, branch.id)?;

        let mut revwalk = repo.revwalk()?;
        revwalk.push(branch.id)?;
        revwalk.hide(merge_base)?;
        revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;

        let mut branch_commits = Vec::new();
        // Walk each branch once and de-dupe globally so commits shared with an older branch
        // are not shown multiple times in the picker, while always retaining this branch tip.
        for oid in revwalk {
            let id = oid?;
            if id == branch.id {
                branch_commits.push(id);
                seen.insert(id);
                continue;
            }

            if seen.insert(id) {
                branch_commits.push(id);
            }
        }

        let total = branch_commits.len();
        // Reverse to tip-first so position is computed from the newest commit downward.
        branch_commits.reverse();

        let mut chunk = Vec::new();
        for (index, oid) in branch_commits.iter().enumerate() {
            let commit = repo.find_commit(*oid)?;
            chunk.push(StackCommit {
                commit_id: *oid,
                branch_name: branch.name.clone(),
                position: (total - index, total),
                message: commit.summary().unwrap_or("").to_string(),
            });
        }
        branch_chunks.push(chunk);
    }

    // Reverse chunks so branches are presented in stack order from newest to oldest.
    branch_chunks.reverse();

    let mut commits = Vec::new();
    for mut chunk in branch_chunks {
        commits.append(&mut chunk);
    }

    Ok(commits)
}

/// Enumerate commits accepted by `kin commit --fixup` for the current branch.
///
/// Feature branches are limited to the current stack. When HEAD is the base
/// branch itself, its first-parent history is eligible too: those commits are
/// excluded from the private stack range by definition, but are precisely the
/// commits a user on the base branch needs to be able to amend or autosquash.
pub fn enumerate_fixup_commits(
    repo: &Repository,
    stack_branches: &[StackBranch],
    upstream_name: &str,
    current_branch_name: &str,
    head_id: Oid,
) -> Result<Vec<StackCommit>> {
    let mut commits = enumerate_stack_commits(repo, stack_branches, upstream_name)?;
    if current_branch_name != upstream_name {
        return Ok(commits);
    }

    let mut ids = Vec::new();
    let mut commit = repo.find_commit(head_id)?;
    loop {
        ids.push(commit.id());
        if commit.parent_count() == 0 {
            break;
        }
        commit = commit.parent(0)?;
    }

    let total = ids.len();
    let mut seen: HashSet<Oid> = commits.iter().map(|commit| commit.commit_id).collect();
    let mut base_commits = Vec::with_capacity(total);
    for (index, id) in ids.into_iter().enumerate() {
        if !seen.insert(id) {
            continue;
        }
        let commit = repo.find_commit(id)?;
        base_commits.push(StackCommit {
            commit_id: id,
            branch_name: upstream_name.to_string(),
            position: (total - index, total),
            message: commit.summary().unwrap_or("").to_string(),
        });
    }
    base_commits.append(&mut commits);
    Ok(base_commits)
}

/// Discover and enumerate the commits accepted by `kin commit --fixup` from the
/// currently checked-out branch. Centralizes the branch / upstream / stack
/// discovery for callers such as the dynamic completion provider.
pub fn enumerate_current_fixup_commits(repo: &Repository) -> Result<Vec<StackCommit>> {
    if repo.head_detached()? {
        return Ok(Vec::new());
    }
    let Some(upstream_name) = crate::trunk::resolve_trunk(repo)? else {
        return Ok(Vec::new());
    };
    let upstream_id = crate::trunk::stack_base(repo, &upstream_name)?;
    let head = repo.head()?;
    let head_id = head.peel_to_commit()?.id();
    let Some(current_branch_name) = head.shorthand() else {
        return Ok(Vec::new());
    };
    let stack_branches = get_stack_branches_for_head(repo, head_id, upstream_id, &upstream_name)?;
    enumerate_fixup_commits(
        repo,
        &stack_branches,
        &upstream_name,
        current_branch_name,
        head_id,
    )
}

/// Discover the complete connected stack, including siblings at unnamed forks.
/// Only shared commits outside upstream connect branches; sharing main alone
/// must not pull unrelated stacks into the tree. Keep lineage-only discovery
/// separate for commands that operate on ancestors and descendants of HEAD.
pub fn get_full_stack_branches_for_head(
    repo: &Repository,
    head_id: Oid,
    upstream_id: Oid,
    upstream_name: &str,
) -> Result<Vec<StackBranch>> {
    // Upstream already contains HEAD, so HEAD has no private history and there
    // is no component to select. Preserve the existing overview of stacks
    // descending from the current base.
    if head_id == upstream_id || repo.graph_descendant_of(upstream_id, head_id)? {
        return get_stack_branches_for_head(repo, head_id, upstream_id, upstream_name);
    }

    // The sources are HEAD followed by every local branch. Walking all of them
    // together, bounded by upstream, costs one traversal of the private history
    // rather than one per branch.
    let mut sources = vec![head_id];
    let mut candidates = Vec::new();
    for entry in repo.branches(Some(git2::BranchType::Local))? {
        let (branch, _) = entry?;
        let Some(name) = branch.name()? else { continue };
        if name == upstream_name {
            continue;
        }
        let Some(id) = branch.get().target() else {
            continue;
        };
        candidates.push(StackBranch {
            name: name.to_string(),
            id,
        });
        sources.push(id);
    }

    let mut walk = repo.revwalk()?;
    walk.set_sorting(git2::Sort::TOPOLOGICAL)?;
    for &id in &sources {
        walk.push(id)?;
    }
    walk.hide(upstream_id)?;
    let order = walk.collect::<std::result::Result<Vec<_>, _>>()?;
    let private: HashSet<Oid> = order.iter().copied().collect();

    // Two sources belong to the same stack when their private histories share a
    // commit. Handing each commit's group down to its parents discovers exactly
    // those overlaps, and the disjoint set closes them transitively.
    let mut seeds: HashMap<Oid, Vec<usize>> = HashMap::new();
    for (index, id) in sources.iter().enumerate() {
        if private.contains(id) {
            seeds.entry(*id).or_default().push(index);
        }
    }
    let mut groups = DisjointSet::new(sources.len());
    let mut group_of: HashMap<Oid, usize> = HashMap::new();
    for oid in order {
        let mut group = group_of.remove(&oid);
        for &seed in seeds.get(&oid).map(Vec::as_slice).unwrap_or_default() {
            group = Some(match group {
                Some(group) => groups.union(group, seed),
                None => seed,
            });
        }
        let Some(group) = group else { continue };
        let group = groups.find(group);
        for parent in repo.find_commit(oid)?.parent_ids() {
            // Commits already in upstream are shared by unrelated stacks, so
            // they must not be allowed to connect them.
            if !private.contains(&parent) {
                continue;
            }
            let merged = match group_of.get(&parent) {
                Some(&existing) => groups.union(existing, group),
                None => group,
            };
            group_of.insert(parent, merged);
        }
    }

    let head_group = groups.find(0);
    let mut branches = Vec::new();
    for (index, branch) in candidates.into_iter().enumerate() {
        // `sources` carries HEAD at index 0, so branch `index` sits at `index + 1`.
        if private.contains(&branch.id) && groups.find(index + 1) == head_group {
            branches.push(branch);
        }
    }
    sort_branches_topologically(repo, &mut branches)?;
    Ok(branches)
}

/// Discover the stack for `head_id` relative to `upstream_id`, computing the
/// merge base internally before delegating to [`get_stack_branches_from_merge_base`].
///
/// This is the shared entry point for callers that only have HEAD and the
/// upstream (e.g. `push`, `pr`) and would otherwise each repeat the
/// `repo.merge_base(...)` + `get_stack_branches_from_merge_base(...)` boilerplate.
pub fn get_stack_branches_for_head(
    repo: &Repository,
    head_id: Oid,
    upstream_id: Oid,
    upstream_name: &str,
) -> Result<Vec<StackBranch>> {
    let merge_base = repo.merge_base(head_id, upstream_id)?;
    get_stack_branches_from_merge_base(repo, merge_base, head_id, upstream_id, upstream_name)
}

/// The branches on HEAD's lineage that upstream does not contain: first those
/// whose tips are in HEAD's private history, then those stacked above HEAD,
/// each group in local branch order. `merge_base` is the merge base of HEAD
/// and upstream; when upstream already contains HEAD, a branch above HEAD
/// must also descend from it.
///
/// Every local branch is related to HEAD by one walk of the private history
/// and one `git for-each-ref` query, never by a graph walk per branch: on a
/// long history with many branches forked from old points, per-branch walks
/// cost branches × history depth.
pub fn get_stack_branches_from_merge_base(
    repo: &Repository,
    merge_base: Oid,
    head_id: Oid,
    upstream_id: Oid,
    upstream_name: &str,
) -> Result<Vec<StackBranch>> {
    let lineage = HeadLineage::new(repo, head_id, upstream_id)?;

    // With HEAD on upstream (e.g. committing on a branch just created there),
    // only branches above HEAD can be in the stack, and they must start at or
    // above the merge base. Callers pass the merge base of HEAD and upstream,
    // which is then HEAD itself and adds nothing to ask.
    let above_merge_base = if lineage.head_is_on_upstream() && merge_base != head_id {
        Some(local_branches_by_ancestry(
            repo,
            &[("--contains", merge_base)],
        )?)
    } else {
        None
    };

    let mut branches = Vec::new();
    let mut above_head = Vec::new();
    for (name, id) in local_branch_tips(repo)? {
        if name == upstream_name {
            continue;
        }

        if lineage.is_private(id) {
            branches.push(StackBranch { name, id });
        } else if lineage.is_above_head(&name)
            && above_merge_base
                .as_ref()
                .is_none_or(|above| above.contains(&name))
        {
            above_head.push(StackBranch { name, id });
        }
    }
    branches.extend(above_head);

    Ok(branches)
}

pub fn get_immediate_successors(
    repo: &Repository,
    current_id: Oid,
    stack_branches: &[StackBranch],
) -> Result<Vec<String>> {
    let mut successors = Vec::new();

    let mut candidates = Vec::new();
    for b in stack_branches {
        if b.id != current_id
            && (current_id.is_zero() || repo.graph_descendant_of(b.id, current_id)?)
        {
            candidates.push(b);
        }
    }

    for candidate in &candidates {
        let mut is_immediate = true;
        for other in &candidates {
            if other.id != candidate.id && repo.graph_descendant_of(candidate.id, other.id)? {
                is_immediate = false;
                break;
            }
        }

        if is_immediate && !successors.contains(&candidate.name) {
            successors.push(candidate.name.clone());
        }
    }

    Ok(successors)
}

pub fn get_stack_tips(repo: &Repository, stack_branches: &[StackBranch]) -> Result<Vec<String>> {
    let mut tips = Vec::new();

    for branch in stack_branches {
        let mut has_descendant = false;
        for other in stack_branches {
            if other.id != branch.id && repo.graph_descendant_of(other.id, branch.id)? {
                has_descendant = true;
                break;
            }
        }

        if !has_descendant && !tips.contains(&branch.name) {
            tips.push(branch.name.clone());
        }
    }

    Ok(tips)
}

pub fn collect_descendants(
    repo: &Repository,
    root_name: &str,
    all_branches: &[StackBranch],
    result: &mut Vec<StackBranch>,
) -> Result<()> {
    let root = all_branches
        .iter()
        .find(|b| b.name == root_name)
        .ok_or_else(|| {
            anyhow!(
                "Branch '{}' not found in stack. Cannot move the upstream branch itself.",
                root_name
            )
        })?;

    result.push(root.clone());
    if !all_branches
        .iter()
        .any(|b| b.id == root.id && b.name != root.name)
    {
        return collect_descendants_of_id(repo, root.id, all_branches, result);
    }

    // Another branch shares the root's tip, so descending from that commit does
    // not make a branch the root's: it belongs to whichever of them it is
    // stacked on.
    let tips = all_branches.iter().map(|b| b.id).collect::<Vec<_>>();
    let mut resolver = ParentResolver::new(repo, TipAncestry::new(repo, &tips)?, all_branches);
    let parents = (0..all_branches.len())
        .map(|index| resolver.parent(index, None))
        .collect::<Result<Vec<_>>>()?;
    for (index, branch) in all_branches.iter().enumerate() {
        let mut parent = parents[index];
        while let Some(ancestor) = parent {
            if all_branches[ancestor].name == root.name {
                if !result.iter().any(|existing| existing.name == branch.name) {
                    result.push(branch.clone());
                }
                break;
            }
            parent = parents[ancestor];
        }
    }
    Ok(())
}

/// The branches at `root_id` together with every branch descending from it.
/// This is the sub-stack an operation that rewrites `root_id` must carry along.
pub fn collect_sub_stack_from_id(
    repo: &Repository,
    root_id: Oid,
    all_branches: &[StackBranch],
) -> Result<Vec<StackBranch>> {
    let mut branches: Vec<_> = all_branches
        .iter()
        .filter(|b| b.id == root_id)
        .cloned()
        .collect();
    collect_descendants_of_id(repo, root_id, all_branches, &mut branches)?;
    Ok(branches)
}

/// Whether `tip` lies on `head`'s own history (at or below it). A rewrite of
/// that history with `--update-refs` moves such a tip by itself; only branches
/// off the path need an explicit replay.
pub fn is_on_history_of(repo: &Repository, tip: Oid, head: Oid) -> Result<bool> {
    Ok(tip == head || repo.graph_descendant_of(head, tip)?)
}

/// Whether a branch forking from `fork_point` is the root of its side path:
/// its stack parent is the fork point itself, or lies outside the fork point's
/// descendants. A parent strictly above the fork point is restacked in its
/// own right and the branch simply follows it.
pub fn is_side_path_root(repo: &Repository, parent: Oid, fork_point: Oid) -> Result<bool> {
    Ok(parent == fork_point || !repo.graph_descendant_of(parent, fork_point)?)
}

pub fn collect_descendants_of_id(
    repo: &Repository,
    root_id: Oid,
    all_branches: &[StackBranch],
    result: &mut Vec<StackBranch>,
) -> Result<()> {
    for b in all_branches {
        if b.id != root_id
            && repo.graph_descendant_of(b.id, root_id)?
            && !result.iter().any(|existing| existing.name == b.name)
        {
            result.push(b.clone());
        }
    }
    Ok(())
}

/// Strict ancestry between a fixed set of branch tips, precomputed so callers
/// can ask "is A a descendant of B?" without walking the graph per question.
///
/// Resolving a stack's shape needs that answer for every pair of tips, and
/// `graph_descendant_of` walks history on each call. One walk over the commits
/// the tips actually span answers them all.
pub(crate) struct TipAncestry {
    /// Slot for every distinct tip, plus the boundary commit.
    slots: HashMap<Oid, usize>,
    /// `descendants[i]` holds the slots of tips that are strict descendants of
    /// the tip in slot `i`.
    descendants: Vec<HashSet<usize>>,
}

impl TipAncestry {
    /// Record ancestry between `tips`, or `None` when they share no common
    /// ancestor to bound the walk and the caller must query the graph directly.
    ///
    /// The bound is `merge_base_octopus`, the commit every tip descends from.
    /// Commits at or below it are shared by all tips and so cannot distinguish
    /// them, which is what keeps the walk proportional to the stack instead of
    /// to all of history. Note that `merge_base_many` is not usable here: it
    /// bases the later commits against a hypothetical merge of them, so it can
    /// return a commit some tip does not descend from, and bounding the walk
    /// there would hide that tip's ancestry and drop edges.
    pub(crate) fn new(repo: &Repository, tips: &[Oid]) -> Result<Option<Self>> {
        let Ok(boundary) = repo.merge_base_octopus(tips) else {
            return Ok(None);
        };
        let mut slots: HashMap<Oid, usize> = HashMap::new();
        for &tip in tips {
            let next = slots.len();
            slots.entry(tip).or_insert(next);
        }

        let mut walk = repo.revwalk()?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL)?;
        for &oid in slots.keys() {
            walk.push(oid)?;
        }
        // Hiding the boundary's parents rather than the boundary itself keeps
        // the boundary in the walk, so tips can still be related to it.
        for parent in repo.find_commit(boundary)?.parent_ids() {
            walk.hide(parent)?;
        }

        // `reach[c]` accumulates the tips that can reach commit `c`. Topological
        // order visits a commit only after every child that feeds into it, so
        // the entry is complete by the time it is read.
        let mut reach: HashMap<Oid, HashSet<usize>> = HashMap::new();
        let mut descendants = vec![HashSet::new(); slots.len()];
        for oid in walk {
            let oid = oid?;
            let mut here = reach.remove(&oid).unwrap_or_default();
            if let Some(&slot) = slots.get(&oid) {
                descendants[slot] = here.clone();
                here.insert(slot);
            }
            if here.is_empty() {
                continue;
            }
            for parent in repo.find_commit(oid)?.parent_ids() {
                reach
                    .entry(parent)
                    .or_default()
                    .extend(here.iter().copied());
            }
        }

        Ok(Some(Self { slots, descendants }))
    }

    /// Ancestry for every branch in `branches`, with `merge_base` included so
    /// it can be compared against too.
    pub(crate) fn for_branches(
        repo: &Repository,
        branches: &[StackBranch],
        merge_base: Oid,
    ) -> Result<Option<Self>> {
        let mut tips = branches.iter().map(|b| b.id).collect::<Vec<_>>();
        tips.push(merge_base);
        Self::new(repo, &tips)
    }

    /// Whether `descendant` is a strict descendant of `ancestor`, matching
    /// `Repository::graph_descendant_of` (a commit is not its own descendant).
    ///
    /// Returns `false` for commits this was not built for.
    pub(crate) fn is_descendant(&self, descendant: Oid, ancestor: Oid) -> bool {
        match (self.slots.get(&descendant), self.slots.get(&ancestor)) {
            (Some(&d), Some(&a)) => self.descendants[a].contains(&d),
            _ => false,
        }
    }
}

/// Disjoint-set over source indices, used to group branches whose private
/// histories overlap.
struct DisjointSet {
    parent: Vec<usize>,
}

impl DisjointSet {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
        }
    }

    fn find(&mut self, mut index: usize) -> usize {
        while self.parent[index] != index {
            self.parent[index] = self.parent[self.parent[index]];
            index = self.parent[index];
        }
        index
    }

    /// Merge the sets holding `a` and `b`, returning the surviving root.
    fn union(&mut self, a: usize, b: usize) -> usize {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.parent[b] = a;
        }
        a
    }
}

/// Whether `a` is a strict descendant of `b`, answered from `ancestry` when it
/// could be built and from the commit graph otherwise.
fn descends(repo: &Repository, ancestry: Option<&TipAncestry>, a: Oid, b: Oid) -> Result<bool> {
    match ancestry {
        Some(ancestry) => Ok(ancestry.is_descendant(a, b)),
        None => is_descendant(repo, a, b),
    }
}

/// How far apart, in seconds, the branch reflog and a HEAD reflog may date the
/// same `git checkout -b`/`git switch -c`. Both entries are written by one
/// command, so they normally carry the same second; the bound has to stay
/// small because every later plain checkout of the branch writes the same
/// `checkout: moving from <source> to <branch>` message with the same commit.
const CREATION_CHECKOUT_WINDOW_SECONDS: i64 = 1;

/// One entry of a worktree's HEAD reflog.
struct HeadEntry {
    message: String,
    id: Oid,
    time: i64,
}

/// Branch creation sources read from the reflogs, each at most once. Reading
/// them costs a reflog per branch, and for branches created at HEAD the HEAD
/// reflog of every worktree, so callers only ask for branches whose tip
/// another branch shares.
#[derive(Default)]
struct CreationSources {
    /// The start point of each branch as the user named it: a branch, another
    /// ref or a commit.
    creations: HashMap<String, Option<String>>,
    head_entries: Option<Vec<HeadEntry>>,
}

impl CreationSources {
    /// The local branch `name` was created from, if Git recorded one.
    fn source(&mut self, repo: &Repository, name: &str) -> Result<Option<String>> {
        if !self.creations.contains_key(name) {
            let creation = self.read_creation(repo, name)?;
            self.creations.insert(name.to_string(), creation);
        }
        Ok(self.creations[name].as_deref().and_then(|source| {
            let source = source.strip_prefix("refs/heads/").unwrap_or(source);
            (source != name).then(|| source.to_string())
        }))
    }

    /// Read the oldest entry of the branch's reflog, `branch: Created from <S>`.
    /// Git names the start point there, except when the branch was created at
    /// HEAD by a checkout, where it writes `HEAD` and the creating worktree's
    /// HEAD reflog holds the branch it moved from.
    fn read_creation(&mut self, repo: &Repository, name: &str) -> Result<Option<String>> {
        let Ok(reflog) = repo.reflog(&format!("refs/heads/{name}")) else {
            return Ok(None);
        };
        let Some(created) = reflog
            .len()
            .checked_sub(1)
            .and_then(|last| reflog.get(last))
        else {
            return Ok(None);
        };
        let Some(source) = created
            .message()
            .and_then(|message| message.strip_prefix("branch: Created from "))
        else {
            return Ok(None);
        };
        if source != "HEAD" {
            return Ok(Some(source.to_string()));
        }

        let id = created.id_new();
        let time = created.committer().when().seconds();
        let prefix = "checkout: moving from ";
        let suffix = format!(" to {name}");
        Ok(self
            .head_entries(repo)?
            .iter()
            .filter(|entry| {
                entry.id == id && (entry.time - time).abs() <= CREATION_CHECKOUT_WINDOW_SECONDS
            })
            .filter_map(|entry| {
                let source = entry
                    .message
                    .strip_prefix(prefix)?
                    .strip_suffix(suffix.as_str())?;
                Some(((entry.time - time).abs(), source))
            })
            .min_by_key(|(distance, _)| *distance)
            .map(|(_, source)| source.to_string()))
    }

    /// The HEAD reflog entries of the main worktree and every linked one, read
    /// on first use. HEAD reflogs are per worktree, and a branch may have been
    /// created in any of them.
    fn head_entries(&mut self, repo: &Repository) -> Result<&[HeadEntry]> {
        if self.head_entries.is_none() {
            let main = Repository::open(repo.commondir())?;
            let mut repos = Vec::new();
            for name in main.worktrees()?.iter().flatten() {
                let worktree = main.find_worktree(name)?;
                // A worktree whose directory is gone cannot be opened, and no
                // branch is being created there.
                if worktree.validate().is_ok() {
                    repos.push(Repository::open_from_worktree(&worktree)?);
                }
            }
            repos.push(main);

            let mut entries = Vec::new();
            for repo in &repos {
                let Ok(reflog) = repo.reflog("HEAD") else {
                    continue;
                };
                // Oldest first: a later checkout of a branch can fall in the
                // second it was created, and a tie goes to the first entry,
                // which is then the checkout that created it.
                for entry in reflog.iter().rev() {
                    if let Some(message) = entry.message() {
                        entries.push(HeadEntry {
                            message: message.to_string(),
                            id: entry.id_new(),
                            time: entry.committer().when().seconds(),
                        });
                    }
                }
            }
            self.head_entries = Some(entries);
        }
        Ok(self.head_entries.as_deref().unwrap_or_default())
    }
}

/// Resolves which branch of a set each branch is stacked on.
///
/// Ancestry decides it, except between branches that share a tip: a branch
/// created on another branch's tip has no commits of its own yet, so only where
/// Git recorded it was created from tells child from parent.
struct ParentResolver<'a> {
    repo: &'a Repository,
    ancestry: Option<TipAncestry>,
    branches: &'a [StackBranch],
    by_name: HashMap<&'a str, usize>,
    by_tip: HashMap<Oid, Vec<usize>>,
    sources: CreationSources,
}

impl<'a> ParentResolver<'a> {
    fn new(
        repo: &'a Repository,
        ancestry: Option<TipAncestry>,
        branches: &'a [StackBranch],
    ) -> Self {
        let mut by_name = HashMap::new();
        let mut by_tip: HashMap<Oid, Vec<usize>> = HashMap::new();
        for (index, branch) in branches.iter().enumerate() {
            by_name.entry(branch.name.as_str()).or_insert(index);
            by_tip.entry(branch.id).or_default().push(index);
        }
        Self {
            repo,
            ancestry,
            branches,
            by_name,
            by_tip,
            sources: CreationSources::default(),
        }
    }

    fn descends(&self, a: Oid, b: Oid) -> Result<bool> {
        descends(self.repo, self.ancestry.as_ref(), a, b)
    }

    fn shares_tip(&self, index: usize) -> bool {
        self.by_tip[&self.branches[index].id].len() > 1
    }

    /// The branch of the set that `index` was created from, at any tip.
    fn source(&mut self, index: usize) -> Result<Option<usize>> {
        let source = self.sources.source(self.repo, &self.branches[index].name)?;
        Ok(source
            .and_then(|source| self.by_name.get(source.as_str()).copied())
            .filter(|&source| source != index))
    }

    /// The branch `index` was created from when it shares `index`'s tip.
    fn same_tip_source(&mut self, index: usize) -> Result<Option<usize>> {
        if !self.shares_tip(index) {
            return Ok(None);
        }
        let tip = self.branches[index].id;
        Ok(self
            .source(index)?
            .filter(|&source| self.branches[source].id == tip))
    }

    /// The branch `index` was created on while sharing its tip. A branch that
    /// was deleted and recreated can make two branches each other's source;
    /// such a cycle names no parent.
    fn same_tip_parent(&mut self, index: usize) -> Result<Option<usize>> {
        let Some(parent) = self.same_tip_source(index)? else {
            return Ok(None);
        };
        let mut seen = HashSet::from([parent]);
        let mut current = parent;
        while let Some(next) = self.same_tip_source(current)? {
            if next == index {
                return Ok(None);
            }
            if !seen.insert(next) {
                break;
            }
            current = next;
        }
        Ok(Some(parent))
    }

    /// The branch `index` is stacked on, or `None` when it sits on the base.
    /// A branch at `merge_base` is the base itself, not a parent.
    fn parent(&mut self, index: usize, merge_base: Option<Oid>) -> Result<Option<usize>> {
        if let Some(parent) = self.same_tip_parent(index)? {
            return Ok(Some(parent));
        }

        let branches = self.branches;
        let branch = &branches[index];
        let mut deepest: Option<usize> = None;
        for (candidate_index, candidate) in branches.iter().enumerate() {
            if candidate.name == branch.name
                || candidate.id == branch.id
                || Some(candidate.id) == merge_base
            {
                continue;
            }
            if !self.descends(branch.id, candidate.id)? {
                continue;
            }
            let deeper = match deepest {
                None => true,
                Some(best) => self.descends(candidate.id, branches[best].id)?,
            };
            if deeper {
                deepest = Some(candidate_index);
            }
        }
        let Some(deepest) = deepest else {
            return Ok(None);
        };

        let tip = self.branches[deepest].id;
        let group = self.by_tip[&tip].clone();
        if group.len() == 1 {
            return Ok(Some(deepest));
        }
        if let Some(source) = self.source(index)?
            && self.branches[source].id == tip
        {
            return Ok(Some(source));
        }
        let mut roots = Vec::new();
        for &member in &group {
            if self.same_tip_parent(member)?.is_none() {
                roots.push(member);
            }
        }
        if let [root] = roots[..] {
            return Ok(Some(root));
        }
        // History alone cannot tell these branches apart; keep the first in
        // the set's order so the answer is at least stable.
        Ok(Some(deepest))
    }

    fn stack_parent(&mut self, index: usize, merge_base: Oid) -> Result<StackParent> {
        Ok(match self.parent(index, Some(merge_base))? {
            Some(parent) => StackParent::Branch {
                name: self.branches[parent].name.clone(),
                id: self.branches[parent].id,
            },
            None => StackParent::Base(merge_base),
        })
    }
}

/// Local branches created on a commit `branch_name` has since been rewritten
/// away from, directly from it or through other branches created on that same
/// commit, and still sitting there, each with that commit. They have no commits
/// of their own, so nothing in their history ties them to the rewritten branch;
/// only the reflogs do. A commit the branch still contains is not left behind:
/// a branch there is part of the branch's history. Nothing is left behind by
/// the trunk, because a branch without commits of its own on the trunk is not
/// stacked on it.
pub fn branches_left_on_rewritten_tip(
    repo: &Repository,
    branch_name: &str,
    branch_tip: Oid,
) -> Result<Vec<(String, Oid)>> {
    if crate::trunk::resolve_trunk(repo)?.as_deref() == Some(branch_name) {
        return Ok(Vec::new());
    }
    let former_tips = read_branch_reflog_ids(repo, branch_name);
    let tips = local_branch_tips(repo)?;
    let tip_of = tips
        .iter()
        .map(|(name, tip)| (name.as_str(), *tip))
        .collect::<HashMap<_, _>>();
    let mut sources = CreationSources::default();
    let mut left = Vec::new();
    for (name, tip) in &tips {
        if name == branch_name
            || *tip == branch_tip
            || !former_tips.contains(tip)
            || repo.graph_descendant_of(branch_tip, *tip)?
        {
            continue;
        }
        let mut current = name.clone();
        let mut seen = HashSet::from([name.clone()]);
        while let Some(source) = sources.source(repo, &current)? {
            if source == branch_name {
                left.push((name.clone(), *tip));
                break;
            }
            if tip_of.get(source.as_str()) != Some(tip) || !seen.insert(source.clone()) {
                break;
            }
            current = source;
        }
    }
    Ok(left)
}

/// The branch in `all_branches` that `branch_name` is stacked on, or
/// `merge_base` when it is stacked on none of them.
pub fn find_parent_in_stack(
    repo: &Repository,
    branch_name: &str,
    all_branches: &[StackBranch],
    merge_base: Oid,
) -> Result<StackParent> {
    let index = all_branches
        .iter()
        .position(|b| b.name == branch_name)
        .ok_or_else(|| anyhow!("Branch '{}' not found in stack.", branch_name))?;

    let ancestry = TipAncestry::for_branches(repo, all_branches, merge_base)?;
    ParentResolver::new(repo, ancestry, all_branches).stack_parent(index, merge_base)
}

/// [`find_parent_in_stack`] for every branch at once, sharing a single ancestry
/// walk instead of repeating one per branch.
pub fn find_parents_in_stack(
    repo: &Repository,
    all_branches: &[StackBranch],
    merge_base: Oid,
) -> Result<HashMap<String, StackParent>> {
    let ancestry = TipAncestry::for_branches(repo, all_branches, merge_base)?;
    let mut resolver = ParentResolver::new(repo, ancestry, all_branches);
    let mut parents = HashMap::new();
    for (index, branch) in all_branches.iter().enumerate() {
        parents.insert(
            branch.name.clone(),
            resolver.stack_parent(index, merge_base)?,
        );
    }
    Ok(parents)
}

fn is_descendant(repo: &Repository, a: Oid, b: Oid) -> Result<bool> {
    repo.graph_descendant_of(a, b).map_err(|e| e.into())
}

pub fn sort_branches_topologically(repo: &Repository, branches: &mut [StackBranch]) -> Result<()> {
    let original = branches.to_vec();
    let mut outgoing = vec![Vec::new(); original.len()];
    let mut indegree = vec![0usize; original.len()];

    // Ordering needs the ancestry of every pair, so precompute it in one walk.
    // That walk has to be bounded by a commit every tip descends from, which is
    // what merge_base_octopus returns. merge_base_many is not interchangeable
    // here: it bases the later commits against a hypothetical merge of them, so
    // its result can be a commit some tip does not descend from, which would
    // prune that tip out of the walk. No common ancestor exists for unrelated
    // tips, in which case fall back to asking the graph pair by pair.
    let tips = original.iter().map(|b| b.id).collect::<Vec<_>>();
    let ancestry = TipAncestry::new(repo, &tips)?;
    let mut resolver = ParentResolver::new(repo, ancestry, &original);

    for (idx, branch) in original.iter().enumerate() {
        for (other_idx, other) in original.iter().enumerate() {
            if idx == other_idx || branch.id == other.id {
                continue;
            }

            if resolver.descends(branch.id, other.id)? {
                outgoing[other_idx].push(idx);
                indegree[idx] += 1;
            }
        }
        // Branches sharing a tip have no ancestry between them; one created on
        // the other's tip still comes after it.
        if let Some(parent_idx) = resolver.same_tip_parent(idx)? {
            outgoing[parent_idx].push(idx);
            indegree[idx] += 1;
        }
    }

    let mut ready = (0..original.len())
        .filter(|&idx| indegree[idx] == 0)
        .collect::<Vec<_>>();
    ready.sort_by(|&a, &b| original[a].name.cmp(&original[b].name));
    let mut ready = VecDeque::from(ready);

    let mut ordered = Vec::with_capacity(original.len());
    while let Some(idx) = ready.pop_front() {
        ordered.push(idx);

        for &child_idx in &outgoing[idx] {
            indegree[child_idx] -= 1;
            if indegree[child_idx] == 0 {
                ready.push_back(child_idx);
            }
        }

        let mut ready_sorted = ready.into_iter().collect::<Vec<_>>();
        ready_sorted.sort_by(|&a, &b| original[a].name.cmp(&original[b].name));
        ready = VecDeque::from(ready_sorted);
    }

    if ordered.len() != original.len() {
        return Err(anyhow!("Failed to topologically sort stack branches."));
    }

    for (slot, idx) in ordered.into_iter().enumerate() {
        branches[slot] = original[idx].clone();
    }

    Ok(())
}

/// For each branch build a map branch_name → base_branch_name.
/// The base is the closest ancestor stack branch that is NOT merged into upstream,
/// or the repo upstream if all ancestors are merged.
pub fn compute_base_map(
    repo: &Repository,
    branches: &[(StackBranch, String)],
    upstream_name: &str,
) -> Result<HashMap<String, String>> {
    let stack_branches = branches
        .iter()
        .map(|(branch, _)| branch.clone())
        .collect::<Vec<_>>();
    let tips = stack_branches.iter().map(|b| b.id).collect::<Vec<_>>();
    let ancestry = TipAncestry::new(repo, &tips)?;
    let mut resolver = ParentResolver::new(repo, ancestry, &stack_branches);
    let mut map = HashMap::new();
    for (index, branch) in stack_branches.iter().enumerate() {
        let base = match resolver.parent(index, None)? {
            Some(parent) => stack_branches[parent].name.clone(),
            None => upstream_name.to_string(),
        };
        map.insert(branch.name.clone(), base);
    }

    Ok(map)
}

pub fn build_parent_maps(
    repo: &Repository,
    sub_stack: &[StackBranch],
    all_branches_in_stack: &[StackBranch],
    merge_base: Oid,
    head_id: Oid,
    current_branch_name: &str,
) -> Result<(HashMap<String, String>, HashMap<String, String>)> {
    let mut parent_id_map = HashMap::new();
    let mut parent_name_map = HashMap::new();

    let parents = find_parents_in_stack(repo, all_branches_in_stack, merge_base)?;
    for sb in sub_stack {
        let parent = &parents[&sb.name];
        let parent_id = parent.id();
        parent_id_map.insert(sb.name.clone(), parent_id.to_string());

        if let Some(parent_name) = parent.branch_name()
            && sub_stack.iter().any(|p| p.name == parent_name)
        {
            parent_name_map.insert(sb.name.clone(), parent_name.to_string());
        } else if parent_id == head_id {
            // or, if parent_id == head_id, map to current_branch_name
            parent_name_map.insert(sb.name.clone(), current_branch_name.to_string());
        }
    }

    Ok((parent_id_map, parent_name_map))
}

#[derive(Clone)]
pub struct VisualBranch {
    pub name: String,
    pub display_name: String,
}

pub fn collect_path_branches(
    repo: &Repository,
    target_tip_id: Oid,
    merge_base: Oid,
    stack_branches: &[StackBranch],
) -> Result<Vec<StackBranch>> {
    let mut path_branches = Vec::new();
    for b in stack_branches {
        let is_on_path = (repo.graph_descendant_of(target_tip_id, b.id)? || target_tip_id == b.id)
            && (repo.graph_descendant_of(b.id, merge_base)? || b.id == merge_base);
        if is_on_path {
            path_branches.push(b.clone());
        }
    }
    Ok(path_branches)
}

pub fn visualize_stack(
    repo: &Repository,
    all_branches: &[StackBranch],
    current_branch_name: Option<&str>,
) -> Result<Vec<VisualBranch>> {
    let mut result = Vec::new();

    let mut stack_branches = all_branches.to_vec();
    sort_branches_topologically(repo, &mut stack_branches)?;

    for b in stack_branches {
        let is_current = current_branch_name == Some(&b.name);
        let prefix = if is_current { "* " } else { "  " };
        result.push(VisualBranch {
            name: b.name.clone(),
            display_name: format!("{}{}", prefix, b.name),
        });
    }

    Ok(result)
}

/// The stack branches already integrated into `upstream`, as `kin sync` would
/// find them: each tip's lineage is cut where [`find_sync_boundary`] stops, so
/// a squash-merged branch counts as well as a merged one. Sorted by name.
pub fn merged_stack_branches(
    repo: &Repository,
    branches: &[StackBranch],
    upstream: &str,
) -> Result<Vec<String>> {
    let mut cache = SyncScanCache::default();
    let mut merged = BTreeSet::new();
    for tip in get_stack_tips(repo, branches)? {
        let boundary = find_sync_boundary_cached(repo, &tip, upstream, branches, &mut cache)?;
        merged.extend(boundary.merged_branches);
    }
    Ok(merged.into_iter().collect())
}

pub struct TreeSyncPlan {
    pub remaining: Vec<String>,
    pub bases: HashMap<String, String>,
    pub parents: HashMap<String, String>,
    pub merged: Vec<String>,
}

/// Plan a branching sync before any refs move. Each child cuts at its original
/// parent tip, unless sync's integration boundary lies beyond that parent.
pub fn plan_tree_sync(
    repo: &Repository,
    branches: &[StackBranch],
    upstream: &str,
    merge_base: Oid,
) -> Result<TreeSyncPlan> {
    let mut ordered = branches.to_vec();
    sort_branches_topologically(repo, &mut ordered)?;
    let mut remaining = Vec::new();
    let mut bases = HashMap::new();
    let mut parents = HashMap::new();
    let mut merged = HashSet::new();
    // One cache for the whole stack: every branch asks the same questions about
    // the same local branches, and they only depend on the merge base and
    // upstream.
    let mut scan_cache = SyncScanCache::default();
    let stack_parents = find_parents_in_stack(repo, branches, merge_base)?;
    for branch in &ordered {
        let boundary =
            find_sync_boundary_cached(repo, &branch.name, upstream, branches, &mut scan_cache)?;
        merged.extend(boundary.merged_branches);
        let Some(mut base) = boundary.old_base else {
            continue;
        };
        let parent = &stack_parents[&branch.name];
        let onto = if let Some(parent_name) = parent.branch_name()
            && remaining.iter().any(|name| name == parent_name)
            && (parent.id() == base || repo.graph_descendant_of(parent.id(), base)?)
        {
            base = parent.id();
            parent_name.to_string()
        } else {
            upstream.to_string()
        };
        bases.insert(branch.name.clone(), base.to_string());
        parents.insert(branch.name.clone(), onto);
        remaining.push(branch.name.clone());
    }
    // A branch retained by any boundary must survive cleanup after the rebases.
    merged.retain(|branch| !bases.contains_key(branch));
    let mut merged: Vec<_> = merged.into_iter().collect();
    merged.sort();
    Ok(TreeSyncPlan {
        remaining,
        bases,
        parents,
        merged,
    })
}

/// Local side branches whose merge base with HEAD lies strictly inside the
/// rewritten range. Descendants of HEAD and branches on HEAD's history are
/// handled by the ordinary restack and update-refs paths respectively.
pub fn branches_forking_from_range(
    repo: &Repository,
    base: Oid,
    head: Oid,
) -> Result<Vec<(StackBranch, Oid)>> {
    let mut branches = Vec::new();
    for entry in repo.branches(Some(git2::BranchType::Local))? {
        let (branch, _) = entry?;
        let Some(name) = branch.name()? else {
            continue;
        };
        let tip = branch.get().peel_to_commit()?.id();
        if tip == head
            || tip == base
            || repo.graph_descendant_of(head, tip)?
            || repo.graph_descendant_of(tip, head)?
            || !repo.graph_descendant_of(tip, base)?
        {
            continue;
        }
        let fork = repo.merge_base(tip, head)?;
        if fork != base {
            branches.push((
                StackBranch {
                    name: name.to_string(),
                    id: tip,
                },
                fork,
            ));
        }
    }
    Ok(branches)
}

#[derive(Default)]
struct BranchGraph {
    nodes: HashSet<String>,
    edges: HashMap<String, HashSet<String>>,
}

/// Discover PR ancestors and descendants of the requested branch from one snapshot.
/// Ancestor siblings are excluded; the upstream branch terminates the base chain.
pub fn discover_pr_connected_stack(
    repo: &Repository,
    branch: &str,
    upstream_name: Option<&str>,
    prs: &HashMap<String, crate::gh::OpenPr>,
) -> Result<(String, Vec<String>)> {
    // A real local branch takes precedence over a remote-like prefix in its name.
    let normalized = if repo.find_branch(branch, git2::BranchType::Local).is_ok() {
        branch.to_string()
    } else {
        let remotes = repo.remotes()?;
        remotes
            .iter()
            .flatten()
            .filter_map(|remote| branch.strip_prefix(&format!("{remote}/")))
            .next()
            .unwrap_or(branch)
            .to_string()
    };
    let branch = normalized.as_str();
    let mut graph = BranchGraph::default();
    graph.nodes.insert(branch.to_string());

    collect_pr_base_chain(branch, upstream_name, &mut graph, prs)?;
    collect_pr_descendants(
        branch,
        &mut graph,
        &mut Vec::new(),
        &mut HashSet::new(),
        prs,
    )?;

    Ok((normalized, topo_sort_branches(&graph)?))
}

fn collect_pr_base_chain(
    branch: &str,
    upstream_name: Option<&str>,
    graph: &mut BranchGraph,
    prs: &HashMap<String, crate::gh::OpenPr>,
) -> Result<()> {
    let mut current = branch.to_string();
    let mut seen_positions = HashMap::new();
    let mut chain = Vec::new();

    loop {
        if let Some(&start_idx) = seen_positions.get(&current) {
            let mut cycle = chain[start_idx..].to_vec();
            cycle.push(current.clone());
            return Err(anyhow!("Detected PR base cycle: {}", cycle.join(" -> ")));
        }

        seen_positions.insert(current.clone(), chain.len());
        chain.push(current.clone());
        graph.nodes.insert(current.clone());

        let Some(pr) = prs.get(&current).filter(|pr| !pr.is_cross_repository) else {
            break;
        };

        let base_branch = pr.base_branch.clone();
        add_edge(graph, &base_branch, &current);

        if upstream_name.is_some_and(|upstream| branch_names_match(&base_branch, upstream)) {
            break;
        }

        current = base_branch;
    }

    Ok(())
}

fn collect_pr_descendants(
    branch: &str,
    graph: &mut BranchGraph,
    path: &mut Vec<String>,
    visited: &mut HashSet<String>,
    prs: &HashMap<String, crate::gh::OpenPr>,
) -> Result<()> {
    if visited.contains(branch) {
        return Ok(());
    }

    if let Some(idx) = path.iter().position(|node| node == branch) {
        let mut cycle = path[idx..].to_vec();
        cycle.push(branch.to_string());
        return Err(anyhow!(
            "Detected PR descendant cycle: {}",
            cycle.join(" -> ")
        ));
    }

    path.push(branch.to_string());

    let mut children: Vec<_> = prs
        .iter()
        .filter(|(_, pr)| !pr.is_cross_repository && pr.base_branch == branch)
        .map(|(head, _)| head)
        .collect();
    children.sort();

    for child in children {
        add_edge(graph, branch, child);
        collect_pr_descendants(child, graph, path, visited, prs)?;
    }

    path.pop();
    visited.insert(branch.to_string());
    Ok(())
}

fn add_edge(graph: &mut BranchGraph, base_branch: &str, child_branch: &str) {
    graph.nodes.insert(base_branch.to_string());
    graph.nodes.insert(child_branch.to_string());
    graph
        .edges
        .entry(base_branch.to_string())
        .or_default()
        .insert(child_branch.to_string());
}

fn topo_sort_branches(graph: &BranchGraph) -> Result<Vec<String>> {
    let mut state = HashMap::<String, VisitState>::new();
    let mut stack = Vec::new();
    let mut order = Vec::new();
    let mut nodes = graph.nodes.iter().cloned().collect::<Vec<_>>();
    nodes.sort();

    for node in nodes {
        visit_branch(&node, graph, &mut state, &mut stack, &mut order)?;
    }

    order.reverse();
    Ok(order)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VisitState {
    Visiting,
    Done,
}

fn visit_branch(
    branch: &str,
    graph: &BranchGraph,
    state: &mut HashMap<String, VisitState>,
    stack: &mut Vec<String>,
    order: &mut Vec<String>,
) -> Result<()> {
    match state.get(branch) {
        Some(VisitState::Done) => return Ok(()),
        Some(VisitState::Visiting) => {
            let start_idx = stack.iter().position(|node| node == branch).unwrap_or(0);
            let mut cycle = stack[start_idx..].to_vec();
            cycle.push(branch.to_string());
            return Err(anyhow!("Detected PR graph cycle: {}", cycle.join(" -> ")));
        }
        None => {}
    }

    state.insert(branch.to_string(), VisitState::Visiting);
    stack.push(branch.to_string());

    let mut children = graph
        .edges
        .get(branch)
        .map(|children| children.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    children.sort();

    for child in children {
        visit_branch(&child, graph, state, stack, order)?;
    }

    stack.pop();
    state.insert(branch.to_string(), VisitState::Done);
    order.push(branch.to_string());
    Ok(())
}

fn branch_names_match(left: &str, right: &str) -> bool {
    normalize_branch_name(left) == normalize_branch_name(right)
}

fn normalize_branch_name(name: &str) -> &str {
    if let Some(rest) = name.strip_prefix("origin/") {
        return rest;
    }
    if let Some(rest) = name.strip_prefix("upstream/") {
        return rest;
    }
    name
}
