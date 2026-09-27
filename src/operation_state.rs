//! What is in progress in a worktree, and the gate a command passes before it
//! changes anything.
//!
//! Three independent facets can be in progress at once:
//!
//! - a Kindra **operation** (move, restack, reorder, sync, commit, absorb, run,
//!   checkout hydration) that records its progress in the worktree's Git
//!   directory so it can be continued or aborted — usually a *paused operation*
//!   waiting for `kin continue` / `kin abort`;
//! - a **native Git operation** (rebase, am, merge, cherry-pick, revert,
//!   bisect) that Git itself is running outside Kindra's control;
//! - **override recovery**: local overrides suspended and awaiting reapply.
//!   It accompanies an operation but is not one.
//!
//! [`query`] reads these facets without side effects: it only reads files in
//! the worktree's Git directory (native state through libgit2) and never
//! writes or spawns Git. Bringing a paused operation's recorded progress in
//! line with what the user did through Git directly is [`reconcile`], a
//! separate step that writes state and therefore requires the [`RepoLock`].
//!
//! Commands call [`ensure_idle`] first, while holding the lock and before any
//! override wrapper, undo snapshot or saved state, so a refused command never
//! leaves anything behind and every refusal speaks with the same words.

use crate::rebase_utils::Operation;
use crate::state_io::RepoLock;
use anyhow::{Result, anyhow};
use git2::{Repository, RepositoryState};
use std::path::PathBuf;

/// One kind of persisted operation state, stored in the worktree's Git
/// directory. File names are part of the on-disk format: an operation paused
/// by an older Kindra must still be continued or aborted after an upgrade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistedOperation {
    /// Every operation driven by the rebase loop (move, restack, reorder,
    /// sync, commit, absorb).
    Rebase,
    Run,
    Hydration,
}

impl PersistedOperation {
    pub const ALL: [Self; 3] = [Self::Rebase, Self::Run, Self::Hydration];

    pub fn path(self, repo: &Repository) -> PathBuf {
        repo.path().join(match self {
            Self::Rebase => "kindra_rebase_state.json",
            Self::Run => "kindra_run_state.json",
            Self::Hydration => "kindra_checkout_state.json",
        })
    }

    fn describe(self) -> &'static str {
        match self {
            Self::Rebase => "a rebase-based operation",
            Self::Run => "an interrupted 'kin run'",
            Self::Hydration => "checkout hydration",
        }
    }
}

/// The Kindra operation facet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KindraOperation {
    None,
    /// A rebase-based operation; `None` when its state cannot be read.
    Rebase(Option<Operation>),
    Run,
    Hydration,
    /// More than one operation is persisted. Nothing may resume either of
    /// them; only `kin abort --clear-state` discards them.
    Conflicting(Vec<PersistedOperation>),
}

/// The native Git operation facet, derived from Git's own view of the
/// worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeOperation {
    None,
    Rebase,
    Am,
    Merge,
    CherryPick,
    Revert,
    Bisect,
}

impl NativeOperation {
    fn git_command(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Rebase => "rebase",
            Self::Am => "am",
            Self::Merge => "merge",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
            Self::Bisect => "bisect",
        }
    }

    /// Which native operation is in progress and how to finish it with Git.
    pub fn advice(self) -> String {
        let command = self.git_command();
        match self {
            Self::None => String::new(),
            Self::Bisect => format!(
                "A native git operation ({command}) is in progress. Finish it with 'git bisect reset'."
            ),
            _ => format!(
                "A native git operation ({command}) is in progress. Finish it with 'git {command} --continue' or 'git {command} --abort'."
            ),
        }
    }
}

/// Override recovery, when local overrides are suspended awaiting reapply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverrideRecovery {
    /// An interrupted `kin overrides remove`: recovery finishes the removal
    /// instead of reapplying.
    pub removing: bool,
}

/// Everything in progress in one worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveOperations {
    pub kindra: KindraOperation,
    pub native: NativeOperation,
    pub override_recovery: Option<OverrideRecovery>,
    /// `kin overrides remove` disabled overrides in this worktree. Not in
    /// progress, but reported alongside.
    pub overrides_disabled: bool,
}

impl ActiveOperations {
    /// Whether a Kindra or native Git operation is in progress.
    pub fn any_operation(&self) -> bool {
        self.kindra != KindraOperation::None || self.native != NativeOperation::None
    }

    fn refusal(&self, repo: &Repository, allow: Allow) -> Option<String> {
        if !allow.kindra_operation
            && let Some(message) = kindra_refusal(&self.kindra)
        {
            return Some(message);
        }
        let native_allowed = match allow.native {
            NativeAllow::Nothing => false,
            NativeAllow::FinishableByCommit => {
                matches!(
                    self.native,
                    NativeOperation::Merge | NativeOperation::CherryPick
                )
            }
            NativeAllow::Anything => true,
        };
        if self.native != NativeOperation::None && !native_allowed {
            return Some(self.native.advice());
        }
        if !allow.override_recovery && self.override_recovery.is_some() {
            return Some(override_recovery_refusal(repo));
        }
        None
    }
}

/// The refusal (and advice) for a Kindra operation facet, if one is present.
pub fn kindra_refusal(kindra: &KindraOperation) -> Option<String> {
    match kindra {
        KindraOperation::None => None,
        KindraOperation::Rebase(operation) => Some(format!(
            "A Kindra operation is already in progress{}. Use 'kin continue' or 'kin abort'.",
            match operation {
                Some(Operation::Move) => " (move or restack)",
                Some(Operation::Reorder) => " (reorder)",
                Some(Operation::Sync) => " (sync)",
                Some(Operation::Commit) => " (commit or absorb)",
                None => "",
            }
        )),
        KindraOperation::Hydration => Some(
            "A Kindra operation is already in progress (checkout hydration). Use 'kin continue' or 'kin abort'."
                .to_string(),
        ),
        KindraOperation::Run => Some(
            "A Kindra operation is already in progress (an interrupted 'kin run'). Use 'kin abort' to restore the working tree."
                .to_string(),
        ),
        KindraOperation::Conflicting(kinds) => Some(conflicting_advice(kinds)),
    }
}

/// The one message for overlapping persisted operations, shared by status,
/// continue, abort and every gate.
pub fn conflicting_advice(kinds: &[PersistedOperation]) -> String {
    let kinds = kinds
        .iter()
        .map(|kind| kind.describe())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Multiple Kindra operations are persisted ({kinds}), so none of them can be continued or aborted. \
         Run 'kin abort --clear-state' to discard Kindra's saved state; Git state and stashes are left intact."
    )
}

pub fn override_recovery_refusal(repo: &Repository) -> String {
    format!(
        "Local overrides are suspended or awaiting recovery. Run 'kin continue' or 'kin abort' in {}.",
        repo.workdir().unwrap_or(repo.path()).display()
    )
}

#[derive(Clone, Copy)]
enum NativeAllow {
    Nothing,
    FinishableByCommit,
    Anything,
}

/// What a command may run alongside. Everything not allowed is refused.
#[derive(Clone, Copy)]
pub struct Allow {
    kindra_operation: bool,
    native: NativeAllow,
    override_recovery: bool,
}

impl Allow {
    /// Commands that rewrite branches or change the working tree.
    pub const NOTHING: Self = Self {
        kindra_operation: false,
        native: NativeAllow::Nothing,
        override_recovery: false,
    };
    /// Commands that leave overlay files alone (rename, reflog), and the
    /// override commands, which are how override recovery is finished.
    pub const OVERRIDE_RECOVERY: Self = Self {
        override_recovery: true,
        ..Self::NOTHING
    };
    /// `kin commit`, which can make the final commit of a resolved native
    /// merge or cherry-pick.
    pub const COMMIT: Self = Self {
        native: NativeAllow::FinishableByCommit,
        ..Self::NOTHING
    };
    /// Commands that publish the stack (push, pr): only a paused Kindra
    /// operation, which leaves the stack half-rewritten, stops them.
    pub const PUBLISH: Self = Self {
        kindra_operation: false,
        native: NativeAllow::Anything,
        override_recovery: true,
    };
}

/// Read what is in progress in `repo`'s worktree. Pure: writes nothing and
/// reconciles nothing, so the answer may include a paused operation that
/// [`reconcile`] would find already complete.
pub fn query(repo: &Repository) -> ActiveOperations {
    ActiveOperations {
        kindra: kindra_operation(repo),
        native: native_operation(repo),
        override_recovery: crate::overrides::recovery_removing(repo)
            .map(|removing| OverrideRecovery { removing }),
        overrides_disabled: crate::overrides::is_disabled(repo),
    }
}

/// The Kindra operation facet. Overlapping state is reported before any file
/// is parsed, so it is detected even when one of them is malformed.
pub fn kindra_operation(repo: &Repository) -> KindraOperation {
    let present: Vec<_> = PersistedOperation::ALL
        .into_iter()
        .filter(|kind| kind.path(repo).exists())
        .collect();
    match present.as_slice() {
        [] => KindraOperation::None,
        [PersistedOperation::Rebase] => KindraOperation::Rebase(
            crate::rebase_utils::load_state(repo)
                .ok()
                .map(|state| state.operation),
        ),
        [PersistedOperation::Run] => KindraOperation::Run,
        [PersistedOperation::Hydration] => KindraOperation::Hydration,
        _ => KindraOperation::Conflicting(present),
    }
}

/// The native Git operation facet. `git am` and the apply backend of `git
/// rebase` share `rebase-apply/`; Git's state tells them apart, and where it
/// cannot, only a rebase records the branch it is rewriting in `head-name`.
pub fn native_operation(repo: &Repository) -> NativeOperation {
    match repo.state() {
        RepositoryState::Clean => NativeOperation::None,
        RepositoryState::Merge => NativeOperation::Merge,
        RepositoryState::Revert | RepositoryState::RevertSequence => NativeOperation::Revert,
        RepositoryState::CherryPick | RepositoryState::CherryPickSequence => {
            NativeOperation::CherryPick
        }
        RepositoryState::Bisect => NativeOperation::Bisect,
        RepositoryState::Rebase
        | RepositoryState::RebaseInteractive
        | RepositoryState::RebaseMerge => NativeOperation::Rebase,
        RepositoryState::ApplyMailbox => NativeOperation::Am,
        RepositoryState::ApplyMailboxOrRebase => {
            if repo.path().join("rebase-apply/head-name").exists() {
                NativeOperation::Rebase
            } else {
                NativeOperation::Am
            }
        }
    }
}

/// Bring a paused rebase-based operation's recorded progress in line with the
/// repository, trimming branches the user finished with Git and clearing the
/// operation once nothing is left. Overlapping state is left untouched.
pub fn reconcile(repo: &Repository, _lock: &RepoLock) -> Result<()> {
    if matches!(kindra_operation(repo), KindraOperation::Rebase(_)) {
        crate::rebase_utils::reconcile_saved_rebase_state(
            repo,
            crate::rebase_utils::ReconcileMode::Passive,
        )?;
    }
    Ok(())
}

/// The gate: reconcile, then refuse unless everything in progress is allowed.
/// Call it first, while holding the command's lock, before any override
/// wrapper, undo snapshot or saved state. A saved state that cannot be
/// reconciled is kept and treated as a paused operation.
pub fn ensure_idle(repo: &Repository, lock: &RepoLock, allow: Allow) -> Result<ActiveOperations> {
    if let Err(err) = reconcile(repo, lock) {
        eprintln!(
            "Warning: failed to reconcile saved Kindra rebase state; treating it as active: {err:#}"
        );
    }
    let active = query(repo);
    match active.refusal(repo, allow) {
        Some(message) => Err(anyhow!(message)),
        None => Ok(active),
    }
}

/// [`ensure_idle`] for commands that do not otherwise hold the lock (push,
/// pr): the lock is held only for the check.
pub fn ensure_idle_now(repo: &Repository, allow: Allow) -> Result<()> {
    let lock = RepoLock::acquire(repo)?;
    ensure_idle(repo, &lock, allow).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn repo() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        (dir, repo)
    }

    fn touch(repo: &Repository, relative: &str) {
        let path = repo.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x\n").unwrap();
    }

    #[test]
    fn idle_worktree_has_nothing_in_progress() {
        let (_dir, repo) = repo();
        let active = query(&repo);
        assert_eq!(active.kindra, KindraOperation::None);
        assert_eq!(active.native, NativeOperation::None);
        assert_eq!(active.override_recovery, None);
        assert!(!active.any_operation());
        assert_eq!(active.refusal(&repo, Allow::NOTHING), None);
    }

    #[test]
    fn native_operations_are_classified_from_git_state() {
        for (files, expected) in [
            (&["rebase-merge/head-name"][..], NativeOperation::Rebase),
            (
                &["rebase-merge/interactive", "rebase-merge/head-name"][..],
                NativeOperation::Rebase,
            ),
            (&["rebase-apply/rebasing"][..], NativeOperation::Rebase),
            (&["rebase-apply/applying"][..], NativeOperation::Am),
            // Neither marker: only a rebase records the branch it rewrites.
            (&["rebase-apply/head-name"][..], NativeOperation::Rebase),
            (&["rebase-apply/patch"][..], NativeOperation::Am),
            (&["MERGE_HEAD"][..], NativeOperation::Merge),
            (&["CHERRY_PICK_HEAD"][..], NativeOperation::CherryPick),
            (
                &["CHERRY_PICK_HEAD", "sequencer/todo"][..],
                NativeOperation::CherryPick,
            ),
            (&["REVERT_HEAD"][..], NativeOperation::Revert),
            (&["BISECT_LOG"][..], NativeOperation::Bisect),
        ] {
            let (_dir, repo) = repo();
            for file in files {
                touch(&repo, file);
            }
            assert_eq!(native_operation(&repo), expected, "{files:?}");
        }
    }

    #[test]
    fn kindra_operations_are_classified_from_persisted_state() {
        let (_dir, repo) = repo();
        fs::write(
            PersistedOperation::Rebase.path(&repo),
            r#"{"operation":"Sync","original_branch":"a","target_branch":"main","remaining_branches":[],"in_progress_branch":null}"#,
        )
        .unwrap();
        assert_eq!(
            kindra_operation(&repo),
            KindraOperation::Rebase(Some(Operation::Sync))
        );

        fs::write(PersistedOperation::Rebase.path(&repo), "{").unwrap();
        assert_eq!(kindra_operation(&repo), KindraOperation::Rebase(None));

        fs::remove_file(PersistedOperation::Rebase.path(&repo)).unwrap();
        fs::write(PersistedOperation::Run.path(&repo), "{").unwrap();
        assert_eq!(kindra_operation(&repo), KindraOperation::Run);

        fs::write(PersistedOperation::Hydration.path(&repo), "{").unwrap();
        assert_eq!(
            kindra_operation(&repo),
            KindraOperation::Conflicting(vec![
                PersistedOperation::Run,
                PersistedOperation::Hydration
            ])
        );
    }

    #[test]
    fn query_writes_nothing() {
        let (_dir, repo) = repo();
        // A completed-looking but unreconciled operation stays exactly as saved.
        let saved = r#"{"operation":"Move","original_branch":"a","target_branch":"b","remaining_branches":[],"in_progress_branch":null}"#;
        fs::write(PersistedOperation::Rebase.path(&repo), saved).unwrap();
        touch(&repo, "MERGE_HEAD");
        let before: Vec<_> = fs::read_dir(repo.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        let active = query(&repo);
        assert_eq!(
            active.kindra,
            KindraOperation::Rebase(Some(Operation::Move))
        );
        assert_eq!(active.native, NativeOperation::Merge);
        let after: Vec<_> = fs::read_dir(repo.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(before, after);
        assert_eq!(
            fs::read_to_string(PersistedOperation::Rebase.path(&repo)).unwrap(),
            saved
        );
    }

    #[test]
    fn allow_sets_decide_what_is_refused() {
        let (_dir, repo) = repo();
        let mut active = query(&repo);

        active.native = NativeOperation::Merge;
        assert!(active.refusal(&repo, Allow::NOTHING).is_some());
        assert_eq!(active.refusal(&repo, Allow::COMMIT), None);
        assert_eq!(active.refusal(&repo, Allow::PUBLISH), None);
        active.native = NativeOperation::Revert;
        assert!(active.refusal(&repo, Allow::COMMIT).is_some());

        active.native = NativeOperation::None;
        active.override_recovery = Some(OverrideRecovery { removing: false });
        assert!(active.refusal(&repo, Allow::NOTHING).is_some());
        assert_eq!(active.refusal(&repo, Allow::OVERRIDE_RECOVERY), None);

        active.kindra = KindraOperation::Hydration;
        for allow in [Allow::NOTHING, Allow::OVERRIDE_RECOVERY, Allow::PUBLISH] {
            assert!(
                active
                    .refusal(&repo, allow)
                    .unwrap()
                    .contains("already in progress")
            );
        }
    }

    #[test]
    fn every_state_has_one_message_naming_its_way_out() {
        assert!(
            NativeOperation::Am
                .advice()
                .contains("'git am --continue' or 'git am --abort'")
        );
        assert!(
            NativeOperation::Bisect
                .advice()
                .contains("'git bisect reset'")
        );
        assert!(
            NativeOperation::CherryPick
                .advice()
                .contains("'git cherry-pick --continue'")
        );
        assert!(
            conflicting_advice(&[PersistedOperation::Rebase, PersistedOperation::Run])
                .contains("kin abort --clear-state")
        );
        assert!(
            kindra_refusal(&KindraOperation::Rebase(None))
                .unwrap()
                .contains("'kin continue' or 'kin abort'")
        );
    }
}
