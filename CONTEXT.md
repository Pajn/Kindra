# Kindra

Kindra (`kin`) manages stacks of dependent Git branches: creating, restacking, syncing and publishing them as one unit.

## Language

### Stacks

**Trunk**:
The long-lived branch a stack is built on and eventually merges into, such as `main`.
_Avoid_: upstream (Git's term for a remote tracking branch), base branch

### Operations

**Operation**:
A Kindra command that changes several branches and records its progress in the worktree so it can be continued or aborted, such as a move, restack, sync or checkout hydration.
_Avoid_: job, transaction, "Kindra-managed rebase"

**Paused operation**:
An operation that stopped, usually on a conflict, and waits for `kin continue` or `kin abort`.
_Avoid_: stuck operation, interrupted rebase

**Native Git operation**:
A multi-step Git command in progress in the worktree outside Kindra's control: a rebase, am, merge, cherry-pick, revert or bisect. It can coexist with a paused operation.
_Avoid_: git state

**Override recovery**:
The period in which a worktree's local overrides are suspended and wait to be reapplied. It accompanies an operation and is not an operation itself.

**Journal**:
The recorded plan and progress of one operation: its steps, the position reached, what was set aside, and the refs it owns. It is what `kin continue` resumes and `kin abort` undoes.
_Avoid_: rebase state, state file

**Step**:
One unit of work in a journal, such as replaying a branch onto a new base, autosquashing a range, or creating a branch at a recorded commit.

**Set-aside**:
Working-tree changes an operation moves out of the way and restores afterwards: the whole tree, only unstaged changes, or staged changes carried to another branch.
_Avoid_: autostash (Git's mechanism, which Kindra does not use; the `--autostash` flags only permit setting tracked changes aside)

**Reconcile**:
Bringing a paused operation's recorded progress in line with the repository after the user has acted with Git directly.
_Avoid_: sync (a separate command), refresh

### Configuration

**Repository config**:
The Kindra settings shared by every worktree of one repository, stored once in the repository's common Git directory.
_Avoid_: worktree config, local config

**Global config**:
The user's Kindra settings that apply to every repository, overridden by the repository config.
_Avoid_: user config
