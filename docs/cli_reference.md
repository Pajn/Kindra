# gits CLI Reference

This document provides a detailed overview of the commands available in `gits` and how to use them effectively for managing stacked git branches.

## Table of Contents

- [Core Concepts](#core-concepts)
- [Command Reference](#command-reference)
  - [commit](#commit)
  - [move](#move)
  - [restack](#restack)
  - [checkout (co)](`#checkout-alias-co`)
  - [push](#push)
  - [split](#split)
  - [Status & Control (status, continue, abort)](#status--control)
- [Shell Completions](#shell-completions)

---

## Core Concepts

`gits` is built around the idea of a **stack** of branches. A stack is a linear sequence of branches where each branch builds on top of the previous one, ultimately originating from a "base" branch (like `main` or `master`).

`gits` automatically identifies your stack by looking for local branches that are descendants of the merge base between your current branch and the base branch.

---

## Command Reference

### `commit`

**Description:** Commits changes and automatically rebases descendant branches in the affected stack.

**Usage:**

```bash
gits commit [git-commit-args]
gits commit --on [<branch>] [git-commit-args]
```

Any arguments you pass to `gits commit` (e.g., `-m "my message"`) are passed directly to `git commit`.

- `--on <branch>`: Commit onto another branch instead of the current one. The next token is consumed as the branch name.
- `--on=`: Open an interactive branch picker for the current stack.
- `--on`: Open the interactive branch picker only when `--on` is the final token.

Parser behavior:
- `gits commit --on feature-a -m "msg"`: valid (`feature-a` is the target branch).
- `gits commit --on -m "msg"`: invalid, because `--on` expects a branch unless used as the final token.
- Use `gits commit --on= -m "msg"` (or `gits commit --on` as the last token) for interactive selection.

When committing onto another branch, `gits` stashes non-staged files, switches to the target branch, commits, rebases dependents (unless you choose not to for an external stack), then returns to your original branch and unstages.

**When to use it:** Use this instead of `git commit` when you are working on a branch that has other branches building on top of it. It saves you from having to manually rebase each dependent branch.

**ASCII-Art Visualization:**

```text
Before commit on 'feature-A':
main -> [A1] -> (feature-A) -> [B1] -> (feature-B) -> [C1] -> (feature-C)

$ gits commit -m "update A"

After gits commit:
main -> [A1] -> [A2] -> (feature-A) -> [B1'] -> (feature-B) -> [C1'] -> (feature-C)
```

*(All descendant branches `feature-B` and `feature-C` are updated automatically.)*

---

### `move`

**Description:** Moves the current branch and all its descendants onto a new target branch.

**Usage:**

```bash
gits move [--onto <target>] [--all]
```

- `--onto <target>`: The branch to move the current stack onto.
- `--all`: If no target is specified, list all local branches to choose from (instead of just branches in the current stack).

**When to use it:** Use this when you want to relocate a whole set of changes to a new base branch (e.g., moving a feature stack from `develop` to `main`).

**ASCII-Art Visualization:**

```text
Before moving 'feature-A' onto 'main':
main -> [M1]
      \-> [D1] -> (develop) -> [A1] -> (feature-A) -> [B1] -> (feature-B)

$ gits move --onto main

After gits move:
main -> [M1] -> [A1'] -> (feature-A) -> [B1'] -> (feature-B)
      \-> [D1] -> (develop)
```

---

### `restack`

**Description:** Rebases the current stack onto the upstream branch (`main`/`master`) in a single rebase using `--update-refs`.

**Usage:**

```bash
gits restack
```

**What it does:**

- Finds the top branch in your current stack.
- Detects the first commit that still needs replaying (while handling lower PRs already landed via merge, rebase/cherry-pick, or squash).
- Checks out the top branch and runs one `git rebase --update-refs --onto <upstream> <old-base> <top>`.

**When to use it:** Use this after one or more lower PRs in your stack have already landed on `main`, and you want to restack all remaining branches in one pass.

**Conflict handling:** If rebase conflicts occur, resolve them and continue with `git rebase --continue` (or cancel with `git rebase --abort`).

---

### `checkout` (alias `co`)

**Description:** Provides an interactive interface to navigate branches in the stack.

**Usage:**

```bash
gits checkout [--all]
gits checkout [subcommand]
```

- `gits co`: Opens an interactive selection menu for branches in the current stack.
- `gits co --all`: Opens an interactive selection menu for all local branches.
- `gits co up`: Checkout the branch immediately "above" the current one in the stack.
- `gits co down`: Checkout the branch immediately "below" the current one in the stack.
- `gits co top`: Checkout the branch at the very top of the current stack.

**When to use it:** Use this for fast, ergonomic navigation without needing to remember branch names.

---

### `push`

**Description:** Pushes all branches in the current stack to their respective upstreams.

**Usage:**

```bash
gits push
```

This command performs an atomic push of all branches in the stack using `force-with-lease` to ensure safety.

**When to use it:** Use this when you've updated multiple branches in your stack (e.g., after a `gits commit` or `gits move`) and want to sync them all to the remote in one go.

---

### `split`

**Description:** Opens your `$EDITOR` to visually manage branch assignments for a series of commits.

**Usage:**

```bash
gits split
```

It generates a list of commits and branches. You can move the `branch <name>` lines to reassign branches to different commits, or add/remove them to create/delete branches.

**When to use it:** Use this when you've made a long series of commits on a single branch and want to "split" them into multiple separate, dependent branches for easier review.

**ASCII-Art Visualization:**

```text
Before split (one branch, multiple commits):
main -> [C1] -> [C2] -> [C3] -> (my-feature)

$ gits split
# In $EDITOR:
[C1] Initial work
branch feature-part-1
[C2] More work
branch feature-part-2
[C3] Final work
branch my-feature

After split:
main -> [C1] -> (feature-part-1) -> [C2] -> (feature-part-2) -> [C3] -> (my-feature)
```

---

### Status & Control

If a `gits commit` or `gits move` operation is interrupted (e.g., due to a merge conflict), use these commands to manage it:

- **`gits status`**: Shows the current state of an in-progress operation, including which branch is currently being rebased and which ones are remaining.
- **`gits continue`**: Resumes the operation after you've resolved conflicts (runs `git rebase --continue` internally).
- **`gits abort`**: Cancels the current operation and cleans up the state.

---

### Shell Completions

**Description:** Generates shell completion scripts for various shells.

**Usage:**

```bash
gits completions <shell>
```

Supported shells: `bash`, `zsh`, `fish`, `powershell`, `elvish`, `nu`.

**Installation Example (Zsh):**

```bash
mkdir -p ~/.zsh/completions
gits completions zsh > ~/.zsh/completions/_gits
fpath=(~/.zsh/completions $fpath)
autoload -Uz compinit && compinit
```
