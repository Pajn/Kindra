//! Set-asides: working-tree changes an operation moves out of the way and
//! restores afterwards. Every stash Kindra pushes, restores or drops goes
//! through this module.
//!
//! A set-aside is a Git stash entry named `<name>-<pid>-<nanos>`, where the
//! command taking it chooses a `kin-` name. Its record ([`SetAside`]) names
//! the entry and the stash commit, so the entry is found again even after
//! other stashes were pushed on top of it, and says how the paused
//! operation's completion and abort restore it.
//!
//! How a restore that does not go cleanly is handled follows the [`Phase`] of
//! the operation's lifecycle it happens in, not the command:
//! [`restore_all`] applies that policy to a journal's set-asides and
//! [`restore`] to a single one.

use anyhow::{Result, anyhow};
use git2::Repository;
use serde::{Deserialize, Serialize};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// What a set-aside took out of the working tree.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Every change: staged and unstaged, and untracked files where the
    /// command takes them.
    WholeTree,
    /// Unstaged changes and untracked files; the staged changes stay in the
    /// index (`--keep-index`).
    UnstagedOnly,
    /// The staged changes `kin commit --on` carries across a branch switch.
    Carry,
}

/// How a paused operation's completion and abort restore a set-aside.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restore {
    /// `git stash apply`: the changes come back unstaged.
    Plain,
    /// `git stash apply --index`: staged changes come back staged. Falls back
    /// to a plain apply, with a warning, only when the attempt left the tree
    /// untouched.
    WithIndex,
    /// Completion brings back only what was unstaged when the changes were
    /// set aside: the difference between the entry's index and its working
    /// tree, applied plainly. For unstaged-only set-asides, whose entry also
    /// holds the staged changes the operation commits. Abort, which undoes
    /// that commit, applies the whole entry plainly instead.
    UnstagedDelta,
}

/// One set-aside, as a journal records it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SetAside {
    pub kind: Kind,
    /// The stash entry's message. A journal saved by Kindra 1.1 or earlier may
    /// hold a `stash@{N}` reference instead, which is used as it is.
    pub stash: String,
    /// The stash commit. The entry is looked up by this id when it is known,
    /// and by its message otherwise (journals saved by Kindra 1.1 or earlier).
    #[serde(default)]
    pub oid: Option<String>,
    pub restore: Restore,
}

/// A journal's set-asides, oldest first. They are restored newest first:
/// `kin commit --on` holds its unstaged changes and, while it switches
/// branches, the staged changes it carries.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(transparent)]
pub struct SetAsides(Vec<SetAside>);

impl From<Vec<SetAside>> for SetAsides {
    fn from(set_asides: Vec<SetAside>) -> Self {
        Self(set_asides)
    }
}

impl Extend<SetAside> for SetAsides {
    fn extend<T: IntoIterator<Item = SetAside>>(&mut self, set_asides: T) {
        self.0.extend(set_asides);
    }
}

impl SetAsides {
    /// Whether nothing is set aside, so no restore is pending.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn push(&mut self, set_aside: SetAside) {
        self.0.push(set_aside);
    }

    /// The set-aside restored next.
    pub fn newest(&self) -> Option<&SetAside> {
        self.0.last()
    }

    /// Remove and return [`SetAsides::newest`].
    pub fn pop(&mut self) -> Option<SetAside> {
        self.0.pop()
    }

    /// The operation's own set-aside: the whole tree or the unstaged changes.
    pub fn changes(&self) -> Option<&SetAside> {
        self.0.iter().rev().find(|s| s.kind != Kind::Carry)
    }

    /// Remove and return the staged changes being carried across a branch
    /// switch.
    pub fn take_carry(&mut self) -> Option<SetAside> {
        let index = self.0.iter().rposition(|s| s.kind == Kind::Carry)?;
        Some(self.0.remove(index))
    }

    /// The set-asides a journal saved by Kindra 1.1 or earlier recorded in
    /// `stash_ref`, `stash_apply_index` and `carry_stash_ref`.
    ///
    /// Every release that recorded `stash_apply_index` set it only for whole
    /// tree stashes; without it (or before it existed) `stash_ref` was the
    /// unstaged changes of `kin commit`, restored plainly. The carry was
    /// always taken after `stash_ref`, so it is newest.
    pub fn from_legacy(
        stash_ref: Option<String>,
        stash_apply_index: bool,
        carry_stash_ref: Option<String>,
    ) -> Self {
        let mut set_asides = Self::default();
        if let Some(stash) = stash_ref {
            let (kind, restore) = if stash_apply_index {
                (Kind::WholeTree, Restore::WithIndex)
            } else {
                (Kind::UnstagedOnly, Restore::Plain)
            };
            set_asides.push(SetAside {
                kind,
                stash,
                oid: None,
                restore,
            });
        }
        if let Some(stash) = carry_stash_ref {
            set_asides.push(SetAside {
                kind: Kind::Carry,
                stash,
                oid: None,
                restore: Restore::WithIndex,
            });
        }
        set_asides
    }
}

/// Where in an operation's lifecycle a set-aside is restored. The phase, not
/// the command, decides what happens when the restore does not go cleanly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// The operation finished its work and restores what it set aside. A
    /// conflicted restore leaves the conflict markers in the tree and keeps
    /// the operation resumable and abortable; a restore that cannot apply
    /// keeps the record and fails, naming the stash entry.
    Completion,
    /// `kin abort` rolled the operation back. A conflicted restore warns; one
    /// that cannot apply fails and keeps the journal.
    Abort,
    /// A command that could not start or finish rolls itself back. Restoring
    /// is best-effort, with the staged state: any problem is a warning that
    /// names the stash entry, and the record stays in the journal (if there
    /// is one) until the changes are back.
    Unwind,
    /// A command that keeps no resumable journal (`kin run`, `kin split`)
    /// restores its set-aside when it ends. Any problem is a warning that
    /// names the stash entry and how to recover it.
    NonResumable,
}

/// How restoring one set-aside ended.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The changes are back and the entry was dropped.
    Restored,
    /// The changes are in the tree as conflict markers, so the entry must not
    /// be applied again; it is kept as a backup.
    ConflictsLeft { backup: String },
    /// The changes are not back, and the entry still holds them. `reason`
    /// says why and names the entry.
    NotRestored { reason: String },
}

/// A saved record of an operation that holds set-asides.
pub trait Journal {
    fn set_asides(&mut self) -> &mut SetAsides;
    /// Persist the journal after one of its set-asides settled.
    fn save(&self, repo: &Repository) -> Result<()>;
}

/// A stash message no other set-aside has: `name`, the process id and a
/// timestamp.
fn stash_message(name: &str) -> Result<String> {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(format!("{name}-{}-{ts}", std::process::id()))
}

/// Set the tracked changes aside as a whole tree, for commands that manage
/// the working tree themselves rather than through `git rebase`, honouring
/// the clean-or-autostash contract. Returns:
/// - `Ok(None)` if the tree is clean (nothing set aside),
/// - `Err(..)` if the tree is dirty and `allowed` (autostash) is off,
/// - `Ok(Some(..))` if the tree was dirty and its changes were set aside.
///
/// Untracked files stay in place. The entry is named `kin-autostash`;
/// `restore` is how the operation's completion and abort bring it back.
pub fn take_tracked(
    repo: &Repository,
    allowed: bool,
    restore: Restore,
) -> Result<Option<SetAside>> {
    if !crate::rebase_utils::working_tree_dirty(repo)? {
        return Ok(None);
    }
    if !allowed {
        return Err(crate::rebase_utils::dirty_working_tree_error());
    }

    let message = stash_message("kin-autostash")?;
    // Capture (rather than inherit) git's output so the internal stash name
    // doesn't leak onto the user's terminal via git's "Saved working
    // directory and index state …" confirmation.
    let output = Command::new("git")
        .args(["stash", "push", "-m", &message])
        .output()?;
    if !output.status.success() {
        return Err(anyhow!("Failed to autostash working tree changes."));
    }

    // `git stash push` exits 0 without creating an entry when it refreshes the
    // index and finds nothing to save ("No local changes to save"). git2's
    // status can report the tree dirty (e.g. a stat-dirty file whose content
    // still matches HEAD) when git stash disagrees, so confirm an entry was
    // actually created before claiming there's something to restore.
    recorded(Kind::WholeTree, message, restore)
}

/// Set changes aside, including untracked files, as `kind`: the whole tree,
/// only the unstaged changes (the staged ones stay in the index), or the
/// staged changes to carry. Untracked local override files stay in place for
/// the operation. `name` names the entry. Returns `None` when there was
/// nothing to set aside.
///
/// Completion restores only what was unstaged of unstaged-only changes, and the
/// whole tree or a carry with their staged state.
pub fn take(repo: &Repository, kind: Kind, name: &str) -> Result<Option<SetAside>> {
    let message = stash_message(name)?;
    let mut cmd = Command::new("git");
    cmd.arg("stash").arg("push");
    if kind == Kind::UnstagedOnly {
        cmd.arg("--keep-index");
    }
    // Capture (rather than inherit) git's output so the internal stash name
    // doesn't leak onto the user's terminal via git's "Saved working directory
    // and index state …" confirmation.
    let output = cmd
        .arg("--include-untracked")
        .arg("-m")
        .arg(&message)
        // Untracked overlay files stay in place for the operation.
        .arg("--")
        .args(crate::overrides::stash_pathspecs(repo)?)
        .output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "Failed to stash working tree changes: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let restore = match kind {
        Kind::UnstagedOnly => Restore::UnstagedDelta,
        Kind::WholeTree | Kind::Carry => Restore::WithIndex,
    };
    // `git stash push` exits 0 without creating an entry when there is nothing
    // to save; confirm an entry was actually created before claiming there's
    // something to restore.
    recorded(kind, message, restore)
}

/// The record of the entry just pushed with `message`, or `None` if Git
/// created none.
fn recorded(kind: Kind, message: String, restore: Restore) -> Result<Option<SetAside>> {
    Ok(find_by_message(&message)?.map(|entry| SetAside {
        kind,
        stash: message,
        oid: Some(entry.oid),
        restore,
    }))
}

/// A set-aside that only a stash message records, such as the one in run
/// state saved by Kindra 1.1 or earlier.
pub fn from_message(kind: Kind, stash: String, restore: Restore) -> SetAside {
    SetAside {
        kind,
        stash,
        oid: None,
        restore,
    }
}

/// Restore `journal`'s set-asides newest first, as `phase` dictates.
///
/// A set-aside leaves the journal once its changes are back in the tree,
/// cleanly or as conflict markers, and Completion and Abort save the journal
/// each time; Unwind and NonResumable never save it. Restoring stops at the
/// first set-aside that does not come back cleanly (Abort alone goes on past
/// conflicts): an older one could overlap it (the carry of `kin commit --on`
/// is also part of its unstaged changes' entry).
///
/// Returns the first outcome that was not [`Outcome::Restored`], or
/// `Restored`. Completion fails on any such outcome, and Abort when a
/// set-aside could not be applied; the other phases only warn.
pub fn restore_all(repo: &Repository, journal: &mut impl Journal, phase: Phase) -> Result<Outcome> {
    let saves = matches!(phase, Phase::Completion | Phase::Abort);
    if phase == Phase::Completion && journal.set_asides().newest().is_some() {
        println!("Restoring set-aside changes...");
    }
    let mut conflicted = None;
    while let Some(set_aside) = journal.set_asides().newest().cloned() {
        let outcome = restore(repo, &set_aside, phase);
        if !matches!(outcome, Outcome::NotRestored { .. }) {
            journal.set_asides().pop();
            if saves {
                journal.save(repo)?;
            }
        }
        match (&outcome, phase) {
            (Outcome::Restored, _) => continue,
            (Outcome::ConflictsLeft { backup }, Phase::Completion) => {
                return Err(anyhow!(
                    "Restoring {} hit conflicts; resolve the conflict markers in the working tree, then run 'kin continue' to finish (or 'kin abort' to roll the operation back). The original changes are also preserved in stash entry '{backup}'.",
                    set_aside.what()
                ));
            }
            (Outcome::NotRestored { reason }, Phase::Completion) => {
                return Err(anyhow!(
                    "{reason} Once that is resolved, run 'kin continue' to restore them, or 'kin abort' to roll the operation back."
                ));
            }
            (Outcome::NotRestored { reason }, Phase::Abort) => {
                return Err(anyhow!(
                    "{reason} Once that is resolved, run 'kin abort' again to restore them."
                ));
            }
            // Abort leaves nothing behind to restore the rest later, so it
            // still tries them: one that cannot apply over the conflicts
            // fails the abort and keeps the journal.
            (Outcome::ConflictsLeft { .. }, Phase::Abort) => conflicted = Some(outcome),
            _ => return Ok(outcome),
        }
    }
    Ok(conflicted.unwrap_or(Outcome::Restored))
}

/// Restore every set-aside in `set_asides` while unwinding a command,
/// newest first, whether or not a journal records them: [`restore_all`] in
/// [`Phase::Unwind`], which never saves and never fails.
pub fn unwind(repo: &Repository, set_asides: &mut SetAsides) -> Outcome {
    struct Unsaved<'a>(&'a mut SetAsides);
    impl Journal for Unsaved<'_> {
        fn set_asides(&mut self) -> &mut SetAsides {
            self.0
        }
        fn save(&self, _: &Repository) -> Result<()> {
            Ok(())
        }
    }
    restore_all(repo, &mut Unsaved(set_asides), Phase::Unwind).unwrap_or_else(|err| {
        Outcome::NotRestored {
            reason: format!("{err:#}"),
        }
    })
}

/// Restore one set-aside in `phase` and drop its entry once the changes are
/// back cleanly. The phase picks how it is applied (the record's
/// [`Restore`], except that Unwind always tries to bring the staged state
/// back and Abort applies an unstaged-only entry whole) and, in Abort, Unwind
/// and NonResumable, prints the warning for an outcome that is not clean.
/// Completion, and Abort's failure, are reported by the caller.
pub fn restore(repo: &Repository, set_aside: &SetAside, phase: Phase) -> Outcome {
    let how = match (phase, set_aside.restore) {
        (Phase::Unwind, _) => Restore::WithIndex,
        (Phase::Abort, Restore::UnstagedDelta) => Restore::Plain,
        (_, restore) => restore,
    };
    let outcome = attempt(repo, set_aside, how).unwrap_or_else(|err| Outcome::NotRestored {
        reason: format!("{err:#}"),
    });
    let what = set_aside.what();
    match (&outcome, phase) {
        (Outcome::Restored, _)
        | (_, Phase::Completion)
        | (Outcome::NotRestored { .. }, Phase::Abort) => {}
        (Outcome::ConflictsLeft { backup }, _) => eprintln!(
            "Warning: restoring {what} left conflicts in the working tree; the stash entry '{backup}' was preserved as a backup."
        ),
        (Outcome::NotRestored { reason }, Phase::Unwind) => eprintln!(
            "Warning: could not restore {what}; they remain in stash entry '{}'. {reason}",
            set_aside.stash
        ),
        (Outcome::NotRestored { reason }, Phase::NonResumable) => eprintln!(
            "Warning: could not restore {what}: {reason} They are still saved on the stash stack, labeled `{}`. Recover them manually: locate it with `git stash list`, then `git stash apply <ref>` (and `git stash drop <ref>` once applied).",
            set_aside.stash
        ),
    }
    outcome
}

impl SetAside {
    /// What the set-aside holds, for messages.
    fn what(&self) -> &'static str {
        match self.kind {
            Kind::Carry => "the staged changes",
            Kind::WholeTree | Kind::UnstagedOnly => "the set-aside changes",
        }
    }
}

/// Apply `set_aside` as `how` says, and drop its entry once it applied
/// cleanly. An error means the changes are not back.
fn attempt(repo: &Repository, set_aside: &SetAside, how: Restore) -> Result<Outcome> {
    let reference = resolve(set_aside)?;
    // Derive revisions from the stash commit's id: `stash@{N}` names whichever
    // entry is at N when each Git command runs.
    let commit = git_output(&["rev-parse", "--verify", &reference])?;
    // Git applies the tracked changes before it restores the untracked files,
    // so an untracked file already in the way leaves a half-applied entry that
    // a retry would apply twice. Refuse before touching anything instead.
    let in_the_way = untracked_in_the_way(repo, &commit)?;
    if !in_the_way.is_empty() {
        return Ok(Outcome::NotRestored {
            reason: format!(
                "Stash entry '{}' would restore untracked files that already exist in the working tree ({}), so nothing was restored. Move them out of the way first.",
                set_aside.stash,
                in_the_way.join(", ")
            ),
        });
    }
    let outcome = match how {
        Restore::Plain => apply(&reference, false, set_aside)?,
        Restore::WithIndex => apply_with_index(&reference, set_aside)?,
        Restore::UnstagedDelta => apply(&unstaged_delta(&commit)?, false, set_aside)?,
    };
    if outcome == Outcome::Restored
        && let Err(err) = drop_entry(set_aside)
    {
        eprintln!("Warning: {err}");
    }
    Ok(outcome)
}

/// Apply a stash with its recorded index state (`--index`) so previously
/// staged hunks come back staged. Distinguishes a conflicted merge (stash
/// content delivered as conflict markers — retrying would double-apply) from
/// an apply that failed before touching the tree.
fn apply_with_index(reference: &str, set_aside: &SetAside) -> Result<Outcome> {
    let before = status_porcelain()?;
    if let Some(outcome) = try_apply(reference, true, set_aside)? {
        return Ok(outcome);
    }
    // `--index` failures come in shapes: the working-tree merge ran and left
    // conflict markers (handled above: retrying any apply would stack the
    // stash on top of itself), a partial application without conflicts, or a
    // refusal before touching anything. Only a provably untouched tree can
    // safely fall back to a plain apply.
    if status_porcelain()? != before {
        return Err(anyhow!(
            "git stash apply --index failed after partially applying stash '{}'. The stash entry is preserved; clean up the partial application and restore it manually with 'git stash apply --index'.",
            set_aside.stash
        ));
    }
    eprintln!(
        "Warning: could not restore the staged state of the set-aside changes; restoring them unstaged."
    );
    apply(reference, false, set_aside)
}

/// `git stash apply [--index] <reference>`, failing unless it applied cleanly
/// or left conflicts.
fn apply(reference: &str, index: bool, set_aside: &SetAside) -> Result<Outcome> {
    try_apply(reference, index, set_aside)?.ok_or_else(|| {
        anyhow!(
            "Failed to apply stashed changes from '{}'.",
            set_aside.stash
        )
    })
}

/// `git stash apply [--index] <reference>`: `None` when it failed without
/// leaving conflicts.
fn try_apply(reference: &str, index: bool, set_aside: &SetAside) -> Result<Option<Outcome>> {
    let mut git = Command::new("git");
    git.arg("stash").arg("apply");
    if index {
        git.arg("--index");
    }
    if git.arg(reference).status()?.success() {
        return Ok(Some(Outcome::Restored));
    }
    if crate::rebase_utils::unmerged_paths_exist()? {
        return Ok(Some(Outcome::ConflictsLeft {
            backup: set_aside.stash.clone(),
        }));
    }
    Ok(None)
}

/// A stash-shaped commit holding only what was unstaged in the stash at
/// `stash` (a commit id): its base is the stash's index, and its own index is that same
/// state, so a plain apply merges in the index-to-worktree difference (and
/// the untracked files) and nothing that was staged.
fn unstaged_delta(stash: &str) -> Result<String> {
    let index = format!("{stash}^2");
    let base = git_output(&[
        "commit-tree",
        &format!("{index}^{{tree}}"),
        "-p",
        &index,
        "-m",
        "kin: set-aside index",
    ])?;
    let mut args = vec![
        "commit-tree".to_string(),
        format!("{stash}^{{tree}}"),
        "-p".to_string(),
        index,
        "-p".to_string(),
        base,
    ];
    if let Some(untracked) = untracked_commit(stash)? {
        args.extend(["-p".to_string(), untracked]);
    }
    args.extend([
        "-m".to_string(),
        "kin: set-aside unstaged changes".to_string(),
    ]);
    git_output(&args.iter().map(String::as_str).collect::<Vec<_>>())
}

/// The commit holding the stash's untracked files, if it has any.
fn untracked_commit(stash: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["rev-parse", "--quiet", "--verify"])
        .arg(format!("{stash}^3"))
        .output()?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string()))
}

/// The stash's untracked files that already exist in the working tree.
fn untracked_in_the_way(repo: &Repository, stash: &str) -> Result<Vec<String>> {
    let Some(untracked) = untracked_commit(stash)? else {
        return Ok(Vec::new());
    };
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow!("Cannot restore set-aside changes in a bare repository."))?;
    // `--full-tree`: from a subdirectory Git would otherwise list only the
    // files under it, relative to it, and the paths are joined to the root.
    let output = Command::new("git")
        .args([
            "ls-tree",
            "--full-tree",
            "-r",
            "-z",
            "--name-only",
            &untracked,
        ])
        .output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "Failed to list the untracked files of the set-aside changes."
        ));
    }
    Ok(output
        .stdout
        .split(|&byte| byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .filter(|path| workdir.join(path).symlink_metadata().is_ok())
        .collect())
}

fn git_output(args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Machine-readable snapshot of the working tree and index state, for
/// detecting whether a failed apply touched anything.
fn status_porcelain() -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(["status", "--porcelain", "-z", "--untracked-files=all"])
        .output()?;
    if !output.status.success() {
        return Err(anyhow!("Failed to read working tree status."));
    }
    Ok(output.stdout)
}

/// Drop `set_aside`'s stash entry.
fn drop_entry(set_aside: &SetAside) -> Result<()> {
    let resolved_ref = resolve(set_aside)?;
    let status = Command::new("git")
        .arg("stash")
        .arg("drop")
        .arg(&resolved_ref)
        .status()?;
    if !status.success() {
        return Err(anyhow!("Failed to drop stash entry '{}'.", set_aside.stash));
    }
    Ok(())
}

/// The `stash@{N}` reference of `set_aside`'s entry: by its stash commit when
/// the record has one, otherwise by its exact message.
fn resolve(set_aside: &SetAside) -> Result<String> {
    if set_aside.stash.starts_with("stash@{") {
        return Ok(set_aside.stash.clone());
    }

    let entry = match &set_aside.oid {
        Some(oid) => entries()?.into_iter().find(|entry| &entry.oid == oid),
        None => find_by_message(&set_aside.stash)?,
    };
    entry
        .map(|entry| entry.reference)
        .ok_or_else(|| anyhow!("Could not locate stash entry '{}'.", set_aside.stash))
}

struct Entry {
    reference: String,
    oid: String,
    message: String,
}

/// The entry created with exactly `message`, if any.
fn find_by_message(message: &str) -> Result<Option<Entry>> {
    Ok(entries()?
        .into_iter()
        .find(|entry| entry.message == message))
}

/// Every stash entry, newest first.
fn entries() -> Result<Vec<Entry>> {
    let output = Command::new("git")
        .arg("stash")
        .arg("list")
        .arg("--format=%gd%x09%H%x09%gs")
        .output()?;
    if !output.status.success() {
        return Err(anyhow!("Failed to list stash entries."));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let reference = fields.next()?.to_string();
            let oid = fields.next()?.to_string();
            let subject = fields.next()?;
            let message = subject
                .split_once(": ")
                .map_or_else(|| subject.trim(), |(_, msg)| msg.trim())
                .to_string();
            Some(Entry {
                reference,
                oid,
                message,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{Kind, Restore, SetAside, SetAsides};

    fn set_aside(kind: Kind, stash: &str, restore: Restore) -> SetAside {
        SetAside {
            kind,
            stash: stash.to_string(),
            oid: None,
            restore,
        }
    }

    #[test]
    fn legacy_whole_tree_stash_restores_with_its_index() {
        let set_asides = SetAsides::from_legacy(Some("kin-absorb-1-2".into()), true, None);
        assert_eq!(
            set_asides,
            SetAsides::from(vec![set_aside(
                Kind::WholeTree,
                "kin-absorb-1-2",
                Restore::WithIndex
            )])
        );
    }

    #[test]
    fn legacy_stash_without_its_index_is_unstaged_only_and_restores_plainly() {
        let set_asides = SetAsides::from_legacy(Some("stash@{0}".into()), false, None);
        assert_eq!(
            set_asides,
            SetAsides::from(vec![set_aside(
                Kind::UnstagedOnly,
                "stash@{0}",
                Restore::Plain
            )])
        );
    }

    #[test]
    fn legacy_carry_is_restored_first() {
        let mut set_asides = SetAsides::from_legacy(
            Some("kin-commit-on-1-2".into()),
            false,
            Some("kin-commit-on-index-1-3".into()),
        );
        assert_eq!(
            set_asides.changes(),
            Some(&set_aside(
                Kind::UnstagedOnly,
                "kin-commit-on-1-2",
                Restore::Plain
            ))
        );
        assert_eq!(set_asides.newest().map(|s| s.kind), Some(Kind::Carry));
        assert_eq!(
            set_asides.pop(),
            Some(set_aside(
                Kind::Carry,
                "kin-commit-on-index-1-3",
                Restore::WithIndex
            ))
        );
        assert_eq!(set_asides.pop().map(|s| s.kind), Some(Kind::UnstagedOnly));
        assert!(set_asides.is_empty());
    }

    #[test]
    fn legacy_journal_without_stashes_sets_nothing_aside() {
        assert!(SetAsides::from_legacy(None, true, None).is_empty());
    }
}
