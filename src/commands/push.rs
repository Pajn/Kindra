use crate::commands::{find_upstream, foreign_base_target, protected_push_targets};
use crate::stack::get_stack_branches_for_head;
use anyhow::{Result, anyhow};
use clap::Args;
use git2::{BranchType, ErrorCode, Repository};
use std::collections::HashSet;
use std::fmt;
use std::process::Command;

/// Per-branch git config opting a branch out of the base-branch push guard.
const ALLOW_BASE_PUSH_CONFIG: &str = "kinAllowBasePush";

#[derive(Args, Default)]
pub struct PushArgs {
    /// Allow this branch to push onto the base branch it tracks, overriding the
    /// safety refusal. Repeatable; each branch must be named explicitly, so this
    /// cannot blanket-disable the guard. Equivalent per-branch config:
    /// `branch.<name>.kinAllowBasePush = true`
    #[arg(long = "allow-base-push", value_name = "BRANCH")]
    pub allow_base_push: Vec<String>,
}

/// Is `branch` exempt from the base-branch push guard?
///
/// Exemption is deliberately per-branch and never global: the flag names branches
/// one at a time (a typo leaves the branch refused rather than opening the guard),
/// and the config key is scoped to a single branch. Note this is intentionally
/// *not* implied by the global `--yes`, which would otherwise silently re-enable
/// the incident in any non-interactive run.
fn base_push_allowed(repo: &Repository, branch: &str, explicit: &[String]) -> bool {
    if explicit.iter().any(|name| name == branch) {
        return true;
    }
    repo.config()
        .and_then(|config| config.get_bool(&format!("branch.{branch}.{ALLOW_BASE_PUSH_CONFIG}")))
        .unwrap_or(false)
}

pub fn push(args: &PushArgs) -> Result<()> {
    let repo = crate::open_repo()?;

    let upstream_name = find_upstream(&repo)?.ok_or_else(|| {
        anyhow!("Could not find a base branch (init.defaultBranch, main, master, or trunk)")
    })?;
    let current_branch_name = repo.head()?.shorthand().map(|name| name.to_string());
    if current_branch_name.as_deref() == Some(&upstream_name) {
        return push_upstream_branch(&repo, &upstream_name);
    }

    let upstream_obj = repo.revparse_single(&upstream_name)?;
    let upstream_id = upstream_obj.id();
    let head_id = repo.head()?.peel_to_commit()?.id();

    let stack_branches = get_stack_branches_for_head(&repo, head_id, upstream_id, &upstream_name)?;
    let branch_names = stack_branches
        .into_iter()
        .map(|sb| sb.name)
        .collect::<Vec<_>>();

    push_stack_branches(&repo, &branch_names, &args.allow_base_push)
}

/// The error for branches whose upstream is a protected base branch, listing each
/// offending mapping and how to repoint it.
///
/// Takes `(branch, remote, target)` triples so the message names the branch's own
/// remote rather than assuming `origin`.
fn protected_target_error(mistracked: &[(String, String, String)]) -> anyhow::Error {
    let mut message = String::from(
        "Refusing to push: these branches track a base branch, so pushing them would force-update it\n",
    );
    for (branch, remote, target) in mistracked {
        message.push_str(&format!(
            "  {branch} -> {remote}/{target} (would overwrite {target} with {branch}'s history)\n"
        ));
    }
    message.push_str(
        "\nThis happens when a branch is created from a base branch's remote ref: git's default\n\
         branch.autoSetupMerge=true makes 'git switch -c <branch> <remote>/<base>' track <base>.\n\
         Repoint each branch at its own remote branch:\n",
    );
    for (branch, remote, _) in mistracked {
        message.push_str(&format!(
            "  git branch --unset-upstream {branch}   # then re-run, or 'git push -u {remote} {branch}'\n"
        ));
    }
    // Callers only build this error for a non-empty set; take the first element
    // explicitly so an empty slice degrades to a message without the worked
    // example rather than panicking on an index.
    if let Some((branch, _, _)) = mistracked.first() {
        message.push_str(&format!(
            "\nIf a mapping above is deliberate (e.g. maintaining a fork's base branch), allow that\n\
             branch explicitly: 'kin push --allow-base-push {branch}', or set\n\
             'git config branch.{branch}.{ALLOW_BASE_PUSH_CONFIG} true'.\n"
        ));
    }
    message.push_str(
        "\nSetting 'git config --global branch.autoSetupMerge simple' prevents it recurring.",
    );
    anyhow!(message)
}

pub(crate) fn push_stack_branches(
    repo: &Repository,
    branches: &[String],
    allow_base_push: &[String],
) -> Result<()> {
    let branch_filter = branches.iter().collect::<HashSet<_>>();
    let protected = protected_push_targets(repo)?;
    let mut branches_to_push = Vec::new();
    let mut branches_without_upstream = Vec::new();
    // Branches whose upstream is a protected base branch. Their tracked destination
    // is unusable (it would rewrite that base), so they are treated as having no
    // upstream and offered to the set-upstream flow below, which pushes
    // `branch:branch`. Each entry is a `(branch, remote, target)` triple.
    let mut branches_tracking_base: Vec<(String, String, String)> = Vec::new();

    for name in branches {
        let branch = repo.find_branch(name, BranchType::Local)?;
        match tracked_push_target(repo, &branch, name.clone())? {
            Some(target) => match target.protected_target(&protected) {
                // An explicitly exempted branch keeps its tracked destination and
                // pushes as configured; `perform_push` labels it in the output so
                // the override is never silent.
                Some(_) if base_push_allowed(repo, name, allow_base_push) => {
                    branches_to_push.push(target);
                }
                Some((remote, base)) => {
                    branches_tracking_base.push((name.clone(), remote.clone(), base.to_string()));
                    branches_without_upstream.push(BranchStatus::tracking_base(
                        name.clone(),
                        base,
                        remote,
                    ));
                }
                None => branches_to_push.push(target),
            },
            None => {
                branches_without_upstream.push(BranchStatus::without_upstream(name.clone()));
            }
        }
    }

    if branches_to_push.is_empty() && branches_without_upstream.is_empty() {
        println!("No branches in stack to push.");
        return Ok(());
    }

    if branches_without_upstream.is_empty() {
        perform_push(repo, branches_to_push, allow_base_push)?;
    } else {
        let mut all_branches = branches_to_push.clone();
        all_branches.extend(branches_without_upstream.clone());
        all_branches.sort_by(|a, b| a.name.cmp(&b.name));

        let options = all_branches
            .iter()
            .filter(|b| b.tracked_ref.is_none() && branch_filter.contains(&b.name))
            .cloned()
            .collect::<Vec<_>>();

        if options.is_empty() {
            // Unreachable while `branch_filter` is built from the same slice these
            // statuses come from (every mis-tracked branch is therefore an option).
            // Kept so a future change to that filter cannot silently drop the check
            // and fall through to a push.
            if !branches_tracking_base.is_empty() {
                return Err(protected_target_error(&branches_tracking_base));
            }
            perform_push(repo, branches_to_push, allow_base_push)?;
            return Ok(());
        }

        let selected = crate::commands::prompt_multi_select(
            "Select branches to set upstream and push (Space to toggle, Enter to confirm):",
            options,
            // Non-interactive: leave untracked branches alone; already-tracked
            // branches still push below.
            crate::commands::Fallback::Default(Vec::new()),
        )?;

        // A mis-tracked branch that was not repointed must not be silently skipped:
        // it is one keystroke away from rewriting a base branch, and non-interactive
        // runs (CI, `kin pr` from a script) never reach the prompt at all. Erroring
        // here pushes nothing, matching the --atomic semantics used below: the stack
        // is mis-configured, so fix it before any of it lands.
        let unfixed = branches_tracking_base
            .iter()
            .filter(|(name, _, _)| !selected.iter().any(|choice| &choice.name == name))
            .cloned()
            .collect::<Vec<_>>();
        if !unfixed.is_empty() {
            return Err(protected_target_error(&unfixed));
        }

        if selected.is_empty() && branches_to_push.is_empty() {
            println!("No branches selected to push.");
            return Ok(());
        }

        let remote_name = resolve_remote(repo)?;
        let mut branches_with_upstream = Vec::new();
        for branch_status in selected {
            branches_with_upstream.push(branch_status.name.clone());
        }

        let mut branches_to_push_with_upstream = Vec::new();
        for name in &branches_with_upstream {
            branches_to_push_with_upstream.push(BranchStatus::with_upstream(
                name.clone(),
                &remote_name,
                name,
            ));
        }

        branches_to_push.extend(branches_to_push_with_upstream);

        perform_push_with_upstream(repo, &branches_with_upstream, &remote_name)?;

        let pushed_names: Vec<&String> = branches_with_upstream.iter().collect();
        let existing_upstream: Vec<BranchStatus> = branches_to_push
            .iter()
            .filter(|b| b.tracked_ref.is_some() && !pushed_names.contains(&&b.name))
            .cloned()
            .collect();

        if !existing_upstream.is_empty() {
            perform_push(repo, existing_upstream, allow_base_push)?;
        }
    }

    Ok(())
}

fn push_upstream_branch(repo: &Repository, upstream_name: &str) -> Result<()> {
    let branch = repo.find_branch(upstream_name, BranchType::Local)?;
    if let Some(target) = tracked_push_target(repo, &branch, upstream_name.to_string())? {
        perform_push(repo, vec![target], &[])
    } else {
        let remote_name = resolve_remote(repo)?;
        perform_push_with_upstream(repo, &[upstream_name.to_string()], remote_name.as_str())
    }
}

#[derive(Clone, Debug)]
struct BranchStatus {
    name: String,
    tracked_remote: Option<String>,
    tracked_ref: Option<String>,
    display_upstream: Option<String>,
    /// Set when the branch's upstream is a protected base branch, naming that base
    /// and the remote it lives on. Such a branch has no usable tracked destination,
    /// so `tracked_ref` is left `None`.
    tracks_base: Option<BaseTracking>,
}

impl BranchStatus {
    fn with_upstream(name: String, remote: &str, remote_ref: &str) -> Self {
        Self {
            name,
            tracked_remote: Some(remote.to_string()),
            tracked_ref: Some(remote_ref.to_string()),
            display_upstream: Some(format!("{}/{}", remote, remote_ref)),
            tracks_base: None,
        }
    }

    fn without_upstream(name: String) -> Self {
        Self {
            name,
            tracked_remote: None,
            tracked_ref: None,
            display_upstream: None,
            tracks_base: None,
        }
    }

    fn tracking_base(name: String, base: &str, remote: &str) -> Self {
        Self {
            name,
            tracked_remote: None,
            tracked_ref: None,
            display_upstream: None,
            tracks_base: Some(BaseTracking {
                base: base.to_string(),
                remote: remote.to_string(),
            }),
        }
    }

    /// The `(remote, base)` this branch would overwrite if pushed to its tracked
    /// destination, or `None` when that destination is its own remote branch.
    fn protected_target<'a>(&self, protected: &'a [String]) -> Option<(&String, &'a str)> {
        let remote = self.tracked_remote.as_ref()?;
        let remote_ref = self.tracked_ref.as_deref()?;
        foreign_base_target(&self.name, remote_ref, protected).map(|base| (remote, base))
    }
}

/// The base branch a mis-tracked branch points at, and the remote it lives on.
#[derive(Clone, Debug)]
struct BaseTracking {
    base: String,
    remote: String,
}

impl fmt::Display for BranchStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(tracking) = &self.tracks_base {
            // Name the (wrong) upstream in full, but do not name the remote the
            // repoint will push to: that is chosen by `resolve_remote`, which
            // prefers `origin`, and may differ from the remote the branch currently
            // tracks. In a fork clone tracking `upstream/main`, pushing to `origin`
            // is correct — so the message must not promise `upstream/<branch>`.
            return write!(
                f,
                "{} (tracks base branch '{}/{}' — select to repoint at its own remote branch)",
                self.name, tracking.remote, tracking.base
            );
        }
        match &self.display_upstream {
            Some(u) => write!(f, "{} -> {}", self.name, u),
            None => write!(f, "{} (no upstream)", self.name),
        }
    }
}

fn resolve_remote(repo: &Repository) -> Result<String> {
    let remotes = repo.remotes()?;
    let remote_list: Vec<String> = remotes.iter().flatten().map(|s| s.to_string()).collect();

    if remote_list.contains(&"origin".to_string()) {
        Ok("origin".to_string())
    } else if remote_list.len() == 1 {
        Ok(remote_list[0].clone())
    } else if remote_list.is_empty() {
        Err(anyhow!("No remotes configured."))
    } else {
        Err(anyhow!(
            "'origin' remote not found and multiple remotes exist. Please specify a remote or rename one to 'origin'."
        ))
    }
}

fn perform_push_with_upstream(repo: &Repository, branches: &[String], remote: &str) -> Result<()> {
    if branches.is_empty() {
        return Ok(());
    }

    println!(
        "Pushing {} branches with upstream to {}...",
        branches.len(),
        remote
    );
    for branch in branches {
        println!("  {branch} -> {remote}/{branch}");
    }
    let mut cmd = Command::new("git");
    cmd.arg("push")
        .arg("--atomic")
        .arg("--force-with-lease")
        .arg("--force-if-includes")
        .arg("-u")
        .arg(remote);

    for branch in branches {
        cmd.arg(format!("{}:{}", branch, branch));
    }

    let output = cmd.output()?;
    // Stream git's own output through so the user still sees progress/errors.
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        if push_rejected_by_lease(&String::from_utf8_lossy(&output.stderr)) {
            let refs: Vec<(String, String)> = branches
                .iter()
                .map(|name| (name.clone(), name.clone()))
                .collect();
            report_push_divergence(repo, remote, &refs);
        }
        return Err(anyhow!("Push failed for remote '{}'", remote));
    }

    Ok(())
}

fn perform_push(
    repo: &Repository,
    branches: Vec<BranchStatus>,
    allow_base_push: &[String],
) -> Result<()> {
    if branches.is_empty() {
        println!("Nothing to push.");
        return Ok(());
    }

    let mut branches_by_remote: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for branch in branches {
        let (Some(remote), Some(remote_ref)) = (branch.tracked_remote, branch.tracked_ref) else {
            continue;
        };

        if let Some((_, refs)) = branches_by_remote
            .iter_mut()
            .find(|(existing_remote, _)| *existing_remote == remote)
        {
            refs.push((branch.name, remote_ref));
        } else {
            branches_by_remote.push((remote, vec![(branch.name, remote_ref)]));
        }
    }

    if branches_by_remote.is_empty() {
        println!("No branches with upstream to push.");
        return Ok(());
    }

    // Last line of defence, covering every caller (`kin push`, `kin pr`, …): never
    // hand git a refspec that rewrites a base branch from a differently-named
    // branch. This must not rely on `--force-with-lease --force-if-includes`
    // catching it — the lease only answers "has the remote moved since I fetched?",
    // and the incident that motivated this guard got past both flags.
    let protected = protected_push_targets(repo)?;
    let mut overridden: HashSet<String> = HashSet::new();
    let mut offending = Vec::new();
    for (remote, refs) in &branches_by_remote {
        for (local_name, remote_ref) in refs {
            let Some(base) = foreign_base_target(local_name, remote_ref, &protected) else {
                continue;
            };
            if base_push_allowed(repo, local_name, allow_base_push) {
                overridden.insert(local_name.clone());
            } else {
                offending.push((local_name.clone(), remote.clone(), base.to_string()));
            }
        }
    }
    if !offending.is_empty() {
        return Err(protected_target_error(&offending));
    }

    for (remote, refs) in branches_by_remote {
        println!("Pushing {} branches to {}...", refs.len(), remote);
        // Show what maps where: a wrong destination is invisible otherwise, and an
        // allowed base push must never look like an ordinary one.
        for (local_name, remote_ref) in &refs {
            if overridden.contains(local_name) {
                println!("  {local_name} -> {remote}/{remote_ref}  (override: allow-base-push)");
            } else {
                println!("  {local_name} -> {remote}/{remote_ref}");
            }
        }
        let mut cmd = Command::new("git");
        cmd.arg("push")
            .arg("--atomic")
            .arg("--force-with-lease")
            .arg("--force-if-includes")
            .arg(&remote);

        for (local_name, remote_ref) in &refs {
            cmd.arg(format!("{}:{}", local_name, remote_ref));
        }

        let output = cmd.output()?;
        print!("{}", String::from_utf8_lossy(&output.stdout));
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        if !output.status.success() {
            if push_rejected_by_lease(&String::from_utf8_lossy(&output.stderr)) {
                report_push_divergence(repo, &remote, &refs);
            }
            return Err(anyhow!("Push failed for remote '{}'", remote));
        }
    }

    Ok(())
}

/// Heuristic: did the push fail specifically because the remote rejected a
/// non-fast-forward / `--force-with-lease` update? Only then is the divergence
/// report relevant — auth, network, and hook failures must not be mislabeled as
/// lease rejections.
fn push_rejected_by_lease(stderr: &str) -> bool {
    const MARKERS: [&str; 6] = [
        "stale info",
        "force-with-lease",
        "force-if-includes",
        "non-fast-forward",
        "fetch first",
        "[rejected]",
    ];
    let lower = stderr.to_ascii_lowercase();
    MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Print a per-branch divergence summary and recovery guidance after a rejected push.
///
/// Kindra pushes with `--force-with-lease --force-if-includes`, so a rejection
/// means the remote holds commits that are not in the local history. The
/// ahead/behind counts are measured against the last-fetched remote-tracking
/// refs, so a branch can still show `↓0` if those refs are stale — hence the
/// advice to fetch before re-inspecting.
fn report_push_divergence(repo: &Repository, remote: &str, refs: &[(String, String)]) {
    eprintln!();
    eprintln!(
        "Push to '{remote}' was rejected. Kindra uses --force-with-lease --force-if-includes, which"
    );
    eprintln!("refuses to overwrite remote commits that are not in your local history.");
    eprintln!("Per-branch status (local vs last-fetched {remote}/…):");
    for (local_name, remote_ref) in refs {
        match branch_ahead_behind(repo, local_name, remote, remote_ref) {
            Ok(Some((ahead, behind))) => {
                eprintln!("  {local_name}: ↑{ahead} ↓{behind} vs {remote}/{remote_ref}");
            }
            Ok(None) => {
                eprintln!(
                    "  {local_name}: no local remote-tracking ref for {remote}/{remote_ref} yet"
                );
            }
            Err(_) => {
                eprintln!("  {local_name}: could not compute divergence");
            }
        }
    }
    eprintln!();
    eprintln!(
        "The remote likely advanced (a teammate pushed, or GitHub's \"Update branch\" was used)."
    );
    eprintln!(
        "Run 'git fetch {remote}', rebase your stack onto the updated base (e.g. 'kin sync'), then push again."
    );
}

/// Ahead/behind of a local branch vs its last-fetched remote-tracking ref.
/// Returns `Ok(None)` when either tip cannot be resolved (e.g. no tracking ref yet).
fn branch_ahead_behind(
    repo: &Repository,
    local_name: &str,
    remote: &str,
    remote_ref: &str,
) -> Result<Option<(usize, usize)>> {
    let local = repo.find_branch(local_name, BranchType::Local)?;
    let Some(local_tip) = local.get().target() else {
        return Ok(None);
    };

    let tracking_ref = format!("refs/remotes/{remote}/{remote_ref}");
    let remote_tip = match repo.find_reference(&tracking_ref) {
        Ok(reference) => reference.target(),
        Err(e) if e.code() == ErrorCode::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let Some(remote_tip) = remote_tip else {
        return Ok(None);
    };

    let (ahead, behind) = repo.graph_ahead_behind(local_tip, remote_tip)?;
    Ok(Some((ahead, behind)))
}

fn tracked_push_target(
    repo: &Repository,
    branch: &git2::Branch<'_>,
    local_name: String,
) -> Result<Option<BranchStatus>> {
    let Ok(upstream_branch) = branch.upstream() else {
        return Ok(None);
    };
    let Some(upstream_ref) = upstream_branch.get().name() else {
        return Ok(None);
    };
    let display_upstream = upstream_branch.name()?.map(str::to_string);
    let Some(local_ref) = branch.get().name() else {
        return Ok(None);
    };
    let remote_name = repo
        .branch_upstream_remote(local_ref)
        .ok()
        .and_then(|buf| buf.as_str().map(|value| value.to_string()));
    let Some(remote_name) = remote_name else {
        return Ok(None);
    };
    let remote_ref = upstream_ref
        .strip_prefix(&format!("refs/remotes/{remote_name}/"))
        .or_else(|| upstream_ref.strip_prefix("refs/heads/"))
        .map(str::to_string)
        .unwrap_or_else(|| upstream_ref.to_string());

    Ok(Some(BranchStatus {
        name: local_name,
        tracked_remote: Some(remote_name),
        tracked_ref: Some(remote_ref),
        display_upstream: display_upstream.or_else(|| Some(upstream_ref.to_string())),
        tracks_base: None,
    }))
}
