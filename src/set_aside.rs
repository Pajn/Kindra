//! Set-asides: working-tree changes an operation moves out of the way and
//! restores afterwards. Every stash Kindra pushes, restores or drops goes
//! through this module.
//!
//! A set-aside is a Git stash entry named `<name>-<pid>-<nanos>`, where the
//! command taking it chooses a `kin-` name. Its record ([`SetAside`]) names
//! the entry and the stash commit, so the entry is found again even after
//! other stashes were pushed on top of it, and says how the paused
//! operation's completion and abort restore it.

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

    /// The operation's own set-aside: the whole tree or the unstaged changes.
    pub fn changes(&self) -> Option<&SetAside> {
        self.0.iter().rev().find(|s| s.kind != Kind::Carry)
    }

    /// Remove and return [`SetAsides::changes`].
    pub fn take_changes(&mut self) -> Option<SetAside> {
        self.take(|kind| kind != Kind::Carry)
    }

    /// The staged changes being carried across a branch switch.
    pub fn carry(&self) -> Option<&SetAside> {
        self.0.iter().rev().find(|s| s.kind == Kind::Carry)
    }

    /// Remove and return [`SetAsides::carry`].
    pub fn take_carry(&mut self) -> Option<SetAside> {
        self.take(|kind| kind == Kind::Carry)
    }

    fn take(&mut self, matches: impl Fn(Kind) -> bool) -> Option<SetAside> {
        let index = self.0.iter().rposition(|s| matches(s.kind))?;
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

/// How an apply that did not fail ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The stash applied cleanly (staged state restored when requested).
    Applied,
    /// The apply merged the stash into the working tree but hit conflicts:
    /// the changes ARE in the tree as conflict markers, so the stash must not
    /// be applied again, but the entry should be preserved as a backup.
    ConflictsLeftInTree,
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
/// Completion and abort restore unstaged-only changes plainly, and the whole
/// tree or a carry with their staged state.
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
        Kind::UnstagedOnly => Restore::Plain,
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

/// A set-aside that only a stash message records, such as the one `kin run`
/// saves in its state.
pub fn from_message(kind: Kind, stash: String, restore: Restore) -> SetAside {
    SetAside {
        kind,
        stash,
        oid: None,
        restore,
    }
}

/// Restore `set_aside` as the journal says ([`SetAside::restore`]). The entry
/// is kept; drop it with [`drop_entry`] once the outcome is settled.
pub fn restore(set_aside: &SetAside) -> Result<Outcome> {
    apply_with_outcome(set_aside, set_aside.restore == Restore::WithIndex)
}

/// Apply `set_aside` plainly (its changes come back unstaged), failing on any
/// conflict. The entry is kept.
pub fn apply(set_aside: &SetAside) -> Result<()> {
    let resolved_ref = resolve(set_aside)?;
    let status = Command::new("git")
        .arg("stash")
        .arg("apply")
        .arg(&resolved_ref)
        .status()?;
    if !status.success() {
        return Err(anyhow!(
            "Failed to apply stashed changes from '{}'. Resolve conflicts and run 'kin continue' or 'kin abort'.",
            set_aside.stash
        ));
    }
    Ok(())
}

/// Apply `set_aside` plainly and drop it, best-effort and silently. Used on
/// error paths where no saved state will restore it later, so the user's
/// changes aren't stranded in the stash list; a failed apply keeps the entry.
pub fn restore_quietly(set_aside: Option<SetAside>) {
    let Some(set_aside) = set_aside else {
        return;
    };
    if apply(&set_aside).is_ok() {
        let _ = drop_entry(&set_aside);
    }
}

/// Best-effort restore of `set_aside` with its staged state on an error path,
/// whatever its record says. Drops the entry only on a clean apply; a
/// conflicted or failed apply keeps it and warns where the changes are.
/// Returns whether the changes came back cleanly (or there were none).
pub fn restore_or_warn(set_aside: Option<SetAside>) -> bool {
    let Some(set_aside) = set_aside else {
        return true;
    };
    match apply_with_outcome(&set_aside, true) {
        Ok(Outcome::Applied) => {
            let _ = drop_entry(&set_aside);
            true
        }
        Ok(Outcome::ConflictsLeftInTree) => {
            eprintln!(
                "Warning: restoring the set-aside changes left conflicts in the working tree; the stash entry '{}' was preserved as a backup.",
                set_aside.stash
            );
            false
        }
        Err(_) => {
            eprintln!(
                "Warning: could not restore the set-aside changes; they remain in stash entry '{}'.",
                set_aside.stash
            );
            false
        }
    }
}

/// Apply a stash, optionally restoring its recorded index state (`--index`) so
/// previously staged hunks come back staged. Distinguishes a conflicted merge
/// (stash content delivered as conflict markers — retrying would double-apply)
/// from an apply that failed before touching the tree.
fn apply_with_outcome(set_aside: &SetAside, restore_index: bool) -> Result<Outcome> {
    let resolved_ref = resolve(set_aside)?;
    if restore_index {
        let before = status_porcelain()?;
        let status = Command::new("git")
            .arg("stash")
            .arg("apply")
            .arg("--index")
            .arg(&resolved_ref)
            .status()?;
        if status.success() {
            return Ok(Outcome::Applied);
        }
        // `--index` failures come in shapes: the working-tree merge ran and
        // left conflict markers (retrying any apply would stack the stash on
        // top of itself), a partial application without conflicts (e.g.
        // untracked files restored before the failure), or a refusal before
        // touching anything. Only a provably untouched tree can safely fall
        // back to a plain apply.
        if crate::rebase_utils::unmerged_paths_exist()? {
            return Ok(Outcome::ConflictsLeftInTree);
        }
        if status_porcelain()? != before {
            return Err(anyhow!(
                "git stash apply --index failed after partially applying stash '{}'. The stash entry is preserved; clean up the partial application and restore it manually with 'git stash apply --index'.",
                set_aside.stash
            ));
        }
        eprintln!(
            "Warning: could not restore the staged state of the set-aside changes; restoring them unstaged."
        );
    }
    let status = Command::new("git")
        .arg("stash")
        .arg("apply")
        .arg(&resolved_ref)
        .status()?;
    if status.success() {
        return Ok(Outcome::Applied);
    }
    if crate::rebase_utils::unmerged_paths_exist()? {
        return Ok(Outcome::ConflictsLeftInTree);
    }
    Err(anyhow!(
        "Failed to apply stashed changes from '{}'. Resolve conflicts and run 'kin continue' or 'kin abort'.",
        set_aside.stash
    ))
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
pub fn drop_entry(set_aside: &SetAside) -> Result<()> {
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
        assert_eq!(set_asides.0.last(), set_asides.carry());
        assert_eq!(
            set_asides.take_carry(),
            Some(set_aside(
                Kind::Carry,
                "kin-commit-on-index-1-3",
                Restore::WithIndex
            ))
        );
        assert!(set_asides.take_changes().is_some());
        assert!(set_asides.is_empty());
    }

    #[test]
    fn legacy_journal_without_stashes_sets_nothing_aside() {
        assert!(SetAsides::from_legacy(None, true, None).is_empty());
    }
}
