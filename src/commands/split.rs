use crate::commands::{CommitInfo, find_upstream};
use crate::stack::{collect_path_branches, get_stack_tips};
use anyhow::{Context, Result, anyhow};
use clap::Args;
use git2::{BranchType, ErrorCode, Oid, Repository};
use std::collections::{HashMap, HashSet};

#[derive(Args, Default)]
pub struct SplitArgs {
    /// Deprecated, has no effect: split never touches uncommitted changes
    #[arg(long, overrides_with = "no_autostash")]
    pub autostash: bool,

    /// Deprecated, has no effect: split never touches uncommitted changes
    #[arg(long = "no-autostash", overrides_with = "autostash")]
    pub no_autostash: bool,
}

/// Split only moves refs, leaving HEAD at its commit, so it never touches the
/// index or working tree: uncommitted changes of any kind stay as they are,
/// and nothing is set aside.
pub fn split(args: &SplitArgs) -> Result<()> {
    for (passed, flag) in [
        (args.autostash, "--autostash"),
        (args.no_autostash, "--no-autostash"),
    ] {
        if passed {
            eprintln!("Note: {flag} has no effect: kin split never touches uncommitted changes.");
        }
    }
    let repo = crate::open_repo()?;
    let lock = crate::state_io::RepoLock::acquire(&repo)?;
    crate::operation_state::ensure_idle(&repo, &lock, crate::operation_state::Allow::NOTHING)?;
    crate::overrides::with_suspended(&repo, false, || split_locked(&repo))
}

fn split_locked(repo: &git2::Repository) -> Result<()> {
    // Snapshot for undo. The guard settles it on every exit — the "no commits"
    // no-op, an editor/parse failure, a successful apply, or a failed one —
    // so `split` can never leave a stale pending snapshot behind.
    let _snapshot = crate::oplog::begin(repo, "split")?;

    let upstream_name = find_upstream(repo)?.ok_or_else(|| {
        anyhow!("Could not find a base branch (init.defaultBranch, main, master, or trunk)")
    })?;
    let upstream_obj = repo.revparse_single(&upstream_name)?;
    let upstream_id = upstream_obj.id();
    let head_obj = repo.revparse_single("HEAD")?;
    let head_id = head_obj.id();

    let merge_base = repo.merge_base(upstream_id, head_id)?;

    let stack_branches = crate::stack::get_stack_branches_from_merge_base(
        repo,
        merge_base,
        head_id,
        upstream_id,
        &upstream_name,
    )?;
    let mut tips = get_stack_tips(repo, &stack_branches)?;
    tips.sort();

    // If there are multiple tips, the user must choose one.
    // If there are no tips (meaning no branches on the stack), we default to HEAD.
    let (target_tip_name, target_tip_id) = match tips.len() {
        0 => ("HEAD".to_string(), head_id),
        1 => (tips[0].clone(), repo.revparse_single(&tips[0])?.id()),
        _ => {
            let selected = crate::commands::prompt_select(
                "Multiple stack tips found. Which path are you splitting?",
                tips,
                crate::commands::Fallback::Require(
                    "Checkout the tip branch you want to split and rerun.",
                ),
            )?;
            let id = repo.revparse_single(&selected)?.id();
            (selected, id)
        }
    };

    // Now we only care about branches that are on the linear path to the target tip.
    let path_branches = collect_path_branches(repo, target_tip_id, merge_base, &stack_branches)?;

    let mut revwalk = repo.revwalk()?;
    revwalk.push(target_tip_id)?;
    revwalk.hide(merge_base)?;
    revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;

    let mut commits = Vec::new();
    let mut commit_ids = HashSet::new();
    for id in revwalk {
        let id = id?;
        let commit = repo.find_commit(id)?;
        let id_str = id.to_string();
        commits.push(CommitInfo {
            id: id_str.clone(),
            summary: commit.summary().unwrap_or("").to_string(),
        });
        commit_ids.insert(id_str);
    }

    if commits.is_empty() {
        println!("No commits to manage between HEAD and {}", upstream_name);
        return Ok(());
    }

    // Map commits to branches (only local branches pointing into our path)
    let mut commit_to_branches: HashMap<String, Vec<String>> = HashMap::new();
    for branch in &path_branches {
        let id_str = branch.id.to_string();
        if commit_ids.contains(&id_str) {
            commit_to_branches
                .entry(id_str)
                .or_default()
                .push(branch.name.clone());
        }
    }

    // Generate buffer
    let mut buffer = String::new();
    for commit in &commits {
        buffer.push_str(&format!("{} {}\n", &commit.id[..7], commit.summary));
        if let Some(branch_names) = commit_to_branches.get(&commit.id) {
            for name in branch_names {
                buffer.push_str(&format!("branch {}\n", name));
            }
        }
    }

    buffer.push_str("\n# kin split\n");
    buffer.push_str("# Move 'branch <name>' rows to reassign branches to commits.\n");
    buffer.push_str("# Add new 'branch <name>' rows to create branches.\n");
    buffer.push_str("# Leave the name blank ('branch') to auto-name it from the commit summary.\n");
    buffer.push_str("# Remove 'branch <name>' rows to delete branches.\n");
    buffer.push_str("# DO NOT edit commit lines (SHA + summary).\n");
    buffer.push_str(&format!("# Base branch: {}\n", upstream_name));
    buffer.push_str(&format!("# Path to tip: {}\n", target_tip_name));

    // Open editor. The buffer is captured into a durable draft so a parse or
    // validation failure below doesn't discard the user's edits. `edit_or_resume`
    // reopens a draft left by an earlier failed run instead of overwriting it
    // with a freshly generated buffer, so re-running `kin split` resumes the
    // user's previous edits.
    let draft = crate::editor::Draft::new(crate::editor::draft_path(
        repo.path(),
        &format!("split-{target_tip_name}"),
    ));
    let edited_buffer = draft.edit_or_resume(&buffer)?;

    match split_from_buffer(repo, &edited_buffer, &commits, &path_branches, commit_ids) {
        Ok(()) => {
            draft.discard();
            Ok(())
        }
        Err(e) => {
            eprintln!(
                "  Your split buffer was saved to {} — fix it and re-run `kin split`.",
                draft.path().display()
            );
            Err(e)
        }
    }
}

/// Resolve a short commit id from the edited buffer to the unique matching
/// commit, applying the same validation as the full-buffer pass: a prefix that
/// matches no commit was moved/modified, and one matching several is ambiguous.
fn resolve_commit_prefix<'a>(commits: &'a [CommitInfo], id_short: &str) -> Result<&'a CommitInfo> {
    let matches: Vec<&CommitInfo> = commits
        .iter()
        .filter(|commit| commit.id.starts_with(id_short))
        .collect();
    match matches.as_slice() {
        [] => Err(anyhow!(
            "Commit '{}' was modified or moved. kin split only supports branch management.",
            id_short
        )),
        [only] => Ok(only),
        many => {
            let candidates: Vec<_> = many.iter().map(|commit| &commit.id[..7]).collect();
            Err(anyhow!(
                "Ambiguous commit prefix {}. Candidates: {:?}",
                id_short,
                candidates
            ))
        }
    }
}

/// Parse the edited split buffer, validate it against the original commit list,
/// and apply the resulting branch layout.
fn split_from_buffer(
    repo: &Repository,
    edited_buffer: &str,
    commits: &[CommitInfo],
    path_branches: &[crate::stack::StackBranch],
    commit_ids: HashSet<String>,
) -> Result<()> {
    // Parse and Validate
    let mut new_commits_short = Vec::new();
    let mut new_branch_map: Vec<(String, String)> = Vec::new(); // (branch_name, commit_id_short)
    // Names already claimed in this buffer, so an auto-named row does not collide
    // with a name assigned to another row.
    let mut chosen_names: HashSet<String> = HashSet::new();
    let mut last_commit_id: Option<String> = None;

    for line in edited_buffer.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // A branch row is `branch <name>`, or bare `branch` to auto-name the
        // branch from the commit it sits on. (Commit rows always start with a SHA,
        // so they never match here.)
        if line == "branch" || line.starts_with("branch ") {
            let rest = line["branch".len()..].trim();
            let Some(id) = last_commit_id.clone() else {
                return Err(anyhow!("A branch row must follow a commit line"));
            };

            let branch_name = if rest.is_empty() {
                // Derive the name only from a uniquely-resolved commit; a missing
                // or ambiguous prefix is a real error, not an empty summary.
                let commit = resolve_commit_prefix(commits, &id)?;
                let base = crate::commands::slugify_subject(&commit.summary).ok_or_else(|| {
                    anyhow!(
                        "Cannot derive a branch name from commit '{}' (empty summary); name the branch explicitly.",
                        id
                    )
                })?;
                crate::commands::disambiguate_branch_name(repo, &base, &chosen_names)?
            } else {
                rest.to_string()
            };

            if branch_name.is_empty() || !git2::Branch::name_is_valid(&branch_name)? {
                return Err(anyhow!(
                    "Invalid branch name '{}' in split editor buffer",
                    branch_name
                ));
            }
            chosen_names.insert(branch_name.clone());
            new_branch_map.push((branch_name, id));
        } else {
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.is_empty() {
                continue;
            }
            let id = parts[0].to_string();
            new_commits_short.push(id.clone());
            last_commit_id = Some(id);
        }
    }

    // Validate commits (order and content must match exactly)
    if new_commits_short.len() != commits.len() {
        return Err(anyhow!(
            "Commit list was modified (count changed). kin split only supports branch management."
        ));
    }

    for (original, new_short) in commits.iter().zip(new_commits_short.iter()) {
        if !original.id.starts_with(new_short) {
            return Err(anyhow!(
                "Commit '{}' was modified or moved. kin split only supports branch management.",
                new_short
            ));
        }
    }

    let mut next_branches: HashMap<String, String> = HashMap::new();
    let mut new_branch_map_full: Vec<(String, String)> = Vec::new();
    for (name, id_short) in &new_branch_map {
        let name = name.clone();
        let id_short = id_short.clone();
        // Map short ID back to full ID
        let matches: Vec<_> = commits
            .iter()
            .filter(|c| c.id.starts_with(&id_short))
            .collect();

        if matches.is_empty() {
            return Err(anyhow!(
                "Could not resolve commit {} for branch {}",
                id_short,
                name
            ));
        } else if matches.len() > 1 {
            let candidates: Vec<_> = matches.iter().map(|c| &c.id[..7]).collect();
            return Err(anyhow!(
                "Ambiguous commit prefix {} for branch {}. Candidates: {:?}",
                id_short,
                name,
                candidates
            ));
        }

        if next_branches.contains_key(&name) {
            return Err(anyhow!("Duplicate branch row for branch {}", name));
        }

        let full_id = matches[0].id.clone();
        next_branches.insert(name.clone(), full_id.clone());
        new_branch_map_full.push((name, full_id));
    }

    // Apply changes
    apply_split(
        repo,
        next_branches,
        new_branch_map_full,
        path_branches.iter().map(|b| b.name.clone()).collect(),
        commit_ids,
    )?;

    Ok(())
}

/// One branch the split changes.
enum BranchChange {
    /// Create the branch (`from` is `None`) or move it to `to`.
    Set {
        name: String,
        from: Option<Oid>,
        to: Oid,
    },
    Delete {
        name: String,
        from: Oid,
    },
}

impl BranchChange {
    fn name(&self) -> &str {
        match self {
            BranchChange::Set { name, .. } | BranchChange::Delete { name, .. } => name,
        }
    }

    fn refname(&self) -> String {
        format!("refs/heads/{}", self.name())
    }

    /// Where the branch points once the split is done.
    fn to(&self) -> Option<Oid> {
        match self {
            BranchChange::Set { to, .. } => Some(*to),
            BranchChange::Delete { .. } => None,
        }
    }
}

/// What HEAD names once the split is done. HEAD's commit never changes, so
/// neither do the index and working tree.
#[derive(Debug, PartialEq, Eq)]
enum HeadChange {
    /// HEAD's branch moves or goes away, so HEAD stays at its commit, detached.
    Detach,
    /// A branch points at HEAD's commit, so HEAD attaches to it.
    Attach(String),
}

/// How applying the split failed.
enum SplitFailure {
    /// The split stopped before writing any ref.
    NothingChanged(anyhow::Error),
    /// Writing the refs failed after some may have been written.
    Partial(anyhow::Error),
}

fn apply_split(
    repo: &Repository,
    next_branches: HashMap<String, String>,
    new_branch_map: Vec<(String, String)>,
    initial_branches: Vec<String>,
    allowed_ids: HashSet<String>,
) -> Result<()> {
    let initial_names: HashSet<String> = initial_branches.into_iter().collect();

    // 1. Decide which branches to create, move, leave or delete, in the
    // buffer's order. Nothing changes yet.
    let mut changes = Vec::new();
    for (name, id) in &new_branch_map {
        let commit_obj = repo.revparse_single(id).context(format!(
            "Failed to resolve target commit {} for branch {}",
            id, name
        ))?;
        let to = commit_obj
            .as_commit()
            .ok_or_else(|| anyhow!("Target {} for branch {} is not a commit", id, name))?
            .id();

        match repo.find_branch(name, BranchType::Local) {
            Ok(existing) => {
                let from = existing.get().target();
                if from == Some(to) {
                    continue;
                }

                // Guard: Only allow moving an existing branch if it was part of the original
                // stack (by name or by pointing to one of the commits in the stack).
                let is_safe = initial_names.contains(name)
                    || from.is_some_and(|t| allowed_ids.contains(&t.to_string()));

                if !is_safe {
                    let confirm_msg = format!(
                        "Branch '{}' already exists and is NOT part of the stack. Do you want to overwrite it?",
                        name
                    );
                    if !crate::commands::prompt_confirm(
                        &confirm_msg,
                        crate::commands::Fallback::Default(false),
                    )? {
                        println!("Skipping branch '{}'", name);
                        continue;
                    }
                }
                changes.push(BranchChange::Set {
                    name: name.clone(),
                    from,
                    to,
                });
            }
            Err(e) if e.code() == ErrorCode::NotFound => {
                changes.push(BranchChange::Set {
                    name: name.clone(),
                    from: None,
                    to,
                });
            }
            Err(e) => {
                return Err(anyhow!(e)
                    .context(format!("Failed to find branch {} during application", name)));
            }
        }
    }

    let mut delete_names: Vec<&String> = initial_names
        .iter()
        .filter(|name| !next_branches.contains_key(*name))
        .collect();
    delete_names.sort();
    for name in delete_names {
        match repo.find_branch(name, BranchType::Local) {
            Ok(branch) => {
                if let Some(from) = branch.get().target() {
                    changes.push(BranchChange::Delete {
                        name: name.clone(),
                        from,
                    });
                }
            }
            // Branch already gone, skip.
            Err(e) if e.code() == ErrorCode::NotFound => {}
            Err(e) => {
                return Err(
                    anyhow!(e).context(format!("Failed to find branch {} for deletion", name))
                );
            }
        }
    }

    let head = head_change(repo, &changes, &new_branch_map)?;

    // 2. Change every ref at once.
    match check_changes(repo, &changes, &head).and_then(|()| commit_changes(repo, &changes, &head))
    {
        Ok(()) => {}
        Err(SplitFailure::NothingChanged(err)) => {
            return Err(anyhow!("{err:#} No branch was changed."));
        }
        // Record whatever was written for `kin undo` now, through a freshly
        // opened repository: after a failed write, `repo`'s cached refs can
        // disagree with the disk (a packed branch whose delete failed looks
        // deleted). The guard in `split_locked` then finds nothing to settle.
        Err(SplitFailure::Partial(err)) => {
            match Repository::open(repo.path()) {
                Ok(fresh) => crate::oplog::finalize(&fresh)?,
                Err(open_err) => {
                    eprintln!("Warning: could not record the split for 'kin undo': {open_err}")
                }
            }
            return Err(anyhow!(
                "{err:#} Some branches may have changed; the split is recorded, so 'kin undo' restores them."
            ));
        }
    }

    // Deletions echo the old tip so a mistaken delete can be undone from the
    // printed value as well as with `kin undo`.
    for change in &changes {
        match change {
            BranchChange::Set { name, from, to } => {
                let verb = if from.is_some() { "Moved" } else { "Created" };
                println!("{verb} branch: {name} -> {}", short(*to));
            }
            BranchChange::Delete { name, from } => {
                println!("Deleted branch: {name} (was {})", short(*from));
                forget_deleted_branch(repo, name);
            }
        }
    }
    match &head {
        Some(HeadChange::Detach) => println!(
            "HEAD is detached at {}: its branch no longer points there.",
            short(repo.head()?.peel_to_commit()?.id())
        ),
        Some(HeadChange::Attach(name)) => println!("HEAD is now on branch {name}."),
        None => {}
    }
    Ok(())
}

fn short(oid: Oid) -> String {
    oid.to_string()[..7].to_string()
}

/// How the split changes what HEAD names, if it does. HEAD stays on its
/// branch while that branch still points at HEAD's commit; otherwise it
/// attaches to the first branch in the buffer that points there once the
/// split is done, or else stays at its commit, detached.
fn head_change(
    repo: &Repository,
    changes: &[BranchChange],
    new_branch_map: &[(String, String)],
) -> Result<Option<HeadChange>> {
    let head_id = repo.head()?.peel_to_commit()?.id();
    let current = current_branch_name(repo)?;
    let final_target = |name: &str| -> Option<Oid> {
        match changes.iter().find(|change| change.name() == name) {
            Some(change) => change.to(),
            None => repo
                .find_branch(name, BranchType::Local)
                .ok()
                .and_then(|branch| branch.get().target()),
        }
    };

    if current
        .as_deref()
        .is_some_and(|name| final_target(name) == Some(head_id))
    {
        return Ok(None);
    }
    // A skipped overwrite branch may still target its old commit even though
    // its desired commit equals HEAD, so look at where each branch ends up.
    let head_id_str = head_id.to_string();
    if let Some((name, _)) = new_branch_map
        .iter()
        .find(|(name, id)| id == &head_id_str && final_target(name) == Some(head_id))
    {
        return Ok(Some(HeadChange::Attach(name.clone())));
    }
    Ok(current.map(|_| HeadChange::Detach))
}

/// Refuse the changes Git would refuse, before any ref is locked: a new
/// branch whose name clashes with another ref's path (`foo/bar` next to
/// `foo`), or moving, deleting or attaching HEAD to a branch another worktree
/// holds.
fn check_changes(
    repo: &Repository,
    changes: &[BranchChange],
    head: &Option<HeadChange>,
) -> Result<(), SplitFailure> {
    let failure = SplitFailure::NothingChanged;
    let mut existing = Vec::new();
    for reference in repo.references().map_err(|e| failure(e.into()))? {
        let reference = reference.map_err(|e| failure(e.into()))?;
        if let Some(name) = reference.name() {
            existing.push(name.to_string());
        }
    }
    let created: Vec<String> = changes
        .iter()
        .filter(|change| matches!(change, BranchChange::Set { from: None, .. }))
        .map(BranchChange::refname)
        .collect();
    for (index, refname) in created.iter().enumerate() {
        let clash = existing
            .iter()
            .chain(created.iter().take(index))
            .find(|other| paths_clash(refname, other));
        if let Some(other) = clash {
            return Err(failure(anyhow!(
                "Cannot create branch '{}': it conflicts with '{other}'.",
                refname.trim_start_matches("refs/heads/")
            )));
        }
    }

    // Another worktree's branch must not move under it, go away, or gain a
    // second worktree. This worktree's own branch is not held elsewhere.
    let mut touched: Vec<(&str, String)> = changes
        .iter()
        .filter_map(|change| match change {
            BranchChange::Set { from: None, .. } => None,
            BranchChange::Set { .. } => Some(("update branch", change.name().to_string())),
            BranchChange::Delete { .. } => Some(("delete branch", change.name().to_string())),
        })
        .collect();
    if let Some(HeadChange::Attach(name)) = head {
        touched.push(("attach HEAD to branch", name.clone()));
    }
    let names: Vec<String> = touched.iter().map(|(_, name)| name.clone()).collect();
    if let Some((branch, held)) =
        crate::rebase_utils::first_held_elsewhere(&names).map_err(failure)?
    {
        let what = touched
            .iter()
            .find(|(_, name)| *name == branch)
            .map_or("change branch", |(what, _)| *what);
        return Err(failure(anyhow!("Cannot {what} '{branch}': it is {held}.")));
    }
    Ok(())
}

/// Whether one ref name is a directory of the other, so both cannot exist.
fn paths_clash(a: &str, b: &str) -> bool {
    a.strip_prefix(b).is_some_and(|rest| rest.starts_with('/'))
        || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('/'))
}

/// Write every branch change and HEAD's in one ref transaction: all the refs
/// are locked before any is written, so a ref that cannot be locked (another
/// Git process holds it) fails the split with nothing changed.
///
/// HEAD's reflog, and that of HEAD's branch when it moves, are written
/// explicitly: otherwise updating HEAD's branch would log the move in HEAD's
/// reflog too, or not, depending on the order the transaction writes them in.
fn commit_changes(
    repo: &Repository,
    changes: &[BranchChange],
    head: &Option<HeadChange>,
) -> Result<(), SplitFailure> {
    let failure = |err: git2::Error| SplitFailure::NothingChanged(err.into());
    let head_id = repo
        .head()
        .and_then(|head| head.peel_to_commit())
        .map_err(failure)?
        .id();
    let current = current_branch_name(repo).map_err(SplitFailure::NothingChanged)?;
    let log_refs = repo
        .config()
        .and_then(|config| config.get_bool("core.logAllRefUpdates"))
        .unwrap_or(true);
    let signature = repo
        .signature()
        .or_else(|_| git2::Signature::now("unknown", "unknown"))
        .map_err(failure)?;
    let mut tx = repo.transaction().map_err(failure)?;

    for change in changes {
        tx.lock_ref(&change.refname()).map_err(failure)?;
    }
    if head.is_some() {
        tx.lock_ref("HEAD").map_err(failure)?;
    }

    for change in changes {
        let refname = change.refname();
        match change {
            BranchChange::Set { from, to, .. } => {
                let verb = if from.is_some() { "move" } else { "create" };
                let message = format!("kin split: {verb} branch at {to}");
                tx.set_target(&refname, *to, Some(&signature), &message)
                    .map_err(failure)?;
                if log_refs && head.is_some() && current.as_deref() == Some(change.name()) {
                    let mut reflog = repo.reflog(&refname).map_err(failure)?;
                    reflog
                        .append(*to, &signature, Some(&message))
                        .map_err(failure)?;
                    tx.set_reflog(&refname, reflog).map_err(failure)?;
                }
            }
            BranchChange::Delete { .. } => tx.remove(&refname).map_err(failure)?,
        }
    }

    if let Some(head) = head {
        let from = current.clone().unwrap_or_else(|| head_id.to_string());
        let message = match head {
            HeadChange::Detach => {
                let message = format!("checkout: moving from {from} to {head_id}");
                tx.set_target("HEAD", head_id, Some(&signature), &message)
                    .map_err(failure)?;
                message
            }
            HeadChange::Attach(name) => {
                let message = format!("checkout: moving from {from} to {name}");
                tx.set_symbolic_target(
                    "HEAD",
                    &format!("refs/heads/{name}"),
                    Some(&signature),
                    &message,
                )
                .map_err(failure)?;
                message
            }
        };
        if log_refs {
            let mut reflog = repo.reflog("HEAD").map_err(failure)?;
            reflog
                .append(head_id, &signature, Some(&message))
                .map_err(failure)?;
            tx.set_reflog("HEAD", reflog).map_err(failure)?;
        }
    }

    tx.commit()
        .map_err(|err| SplitFailure::Partial(anyhow!(err).context("Failed to update the branches")))
}

/// Remove what Git removes with a deleted branch besides the ref: its reflog
/// and its `branch.<name>` configuration. The branch is gone either way, so a
/// failure is only a warning.
fn forget_deleted_branch(repo: &Repository, name: &str) {
    let result = (|| -> Result<()> {
        repo.reflog_delete(&format!("refs/heads/{name}"))?;
        let mut config = repo.config()?.open_level(git2::ConfigLevel::Local)?;
        let prefix = format!("branch.{name}.");
        let mut keys = Vec::new();
        config.entries(None)?.for_each(|entry| {
            if let Some(key) = entry.name()
                && key
                    .strip_prefix(&prefix)
                    .is_some_and(|variable| !variable.contains('.'))
            {
                keys.push(key.to_string());
            }
        })?;
        // A key's values can be spread over several sections of the same
        // name; one `remove_multivar` removes them all.
        keys.sort();
        keys.dedup();
        for key in keys {
            match config.remove_multivar(&key, ".*") {
                Err(err) if err.code() != ErrorCode::NotFound => return Err(err.into()),
                _ => {}
            }
        }
        Ok(())
    })();
    if let Err(err) = result {
        eprintln!(
            "Warning: could not remove the reflog and configuration of deleted branch {name}: {err:#}"
        );
    }
}

fn current_branch_name(repo: &Repository) -> Result<Option<String>> {
    if repo.head_detached()? {
        Ok(None)
    } else {
        Ok(repo.head()?.shorthand().map(|s| s.to_string()))
    }
}
