# gits CLI Reference

This document provides a detailed overview of the commands available in `gits` and how to use them effectively for managing stacked git branches.

## Table of Contents

- [Core Concepts](#core-concepts)
- [Command Reference](#command-reference)
  - [commit](#commit)
  - [move](#move)
  - [sync](#sync)
  - [restack](#restack)
  - [checkout (co)](#checkout-alias-co)
  - [push](#push)
  - [pr](#pr)
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

**Description:** Commits changes and automatically rebases descendant branches in the affected stack in a single pass using `--update-refs` (Requires Git >= 2.38.0).

**Usage:**

```bash
gits commit [git-commit-args]
gits commit --on [<branch>] [git-commit-args]
```

Any arguments you pass to `gits commit` (e.g., `-m "my message"`) are passed directly to `git commit`.

- `--on <branch>`: Commit onto another branch instead of the current one. The next token is consumed as the branch name.
- `--on=`: Open an interactive branch picker for the current stack.
- `--on`: Open the interactive branch picker only when `--on` is the final token.
- `--autostash`: Allow the descendant rebase phase to use Git autostash.
- `--no-autostash`: Disable Git autostash even if configured globally or for the repo.

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

**Description:** Moves the current branch and all its descendants onto a new target branch in a single pass using `--update-refs` (Requires Git >= 2.38.0).

**Usage:**

```bash
gits move [--onto <target>] [--all] [--autostash|--no-autostash]
```

- `--onto <target>`: The branch to move the current stack onto.
- `--all`: If no target is specified, list all local branches to choose from (instead of just branches in the current stack).
- `--autostash`: Allow the rebase loop to use Git autostash.
- `--no-autostash`: Disable Git autostash even if configured globally or for the repo.

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

### `sync`

**Description:** Rebases the current stack onto the resolved upstream branch in a single rebase using `--update-refs` and automatically cleans up merged branches.

**Usage:**

```bash
gits sync [--force] [--no-delete] [--autostash|--no-autostash]
```

**Arguments:**
- `--force`: Force the sync even if branches in the stack are checked out in other worktrees.
- `--no-delete`: Do not automatically delete branches that have already been integrated into the upstream branch.
- `--autostash`: Allow the sync rebase to use Git autostash.
- `--no-autostash`: Disable Git autostash even if configured globally or for the repo.

**What it does:**

- Finds the resolved upstream/base branch using `find_upstream()` logic. This detection covers `.git/gits.toml`, `init.defaultBranch`, common names like `main`/`master`/`trunk`, and remote-qualified bases.
- Finds the top branch in your current stack.
- Detects the first commit that still needs replaying (while handling lower PRs already landed via merge, rebase/cherry-pick, or squash).
- Checks out the top branch and runs one `git rebase --update-refs --onto <upstream> <old-base> <top>`.
- **Automatically deletes local branches** in the stack that are already merged into the upstream branch (unless `--no-delete` is used).
- If your current branch is deleted because it was merged, `gits` automatically switches you to the upstream branch.

**When to use it:** Use this after one or more lower PRs in your stack have already landed on the upstream branch, and you want to sync all remaining branches and clean up the merged ones in one pass.

**Conflict handling:** If rebase conflicts occur, resolve them and continue with `git rebase --continue` (or cancel with `git rebase --abort`).

---

### `restack`

**Description:** Automatically identifies and repairs "floating" branches that were based on an old version of the current branch (e.g., after an `amend` or `rebase`).

**Usage:**

```bash
gits restack [--history-limit <n>] [--autostash|--no-autostash]
```

**What it does:**
- Scans all local branches for those whose history includes a commit that "matches" the current `HEAD` (by patch-id or tree-hash) but is not part of the current branch's ancestry.
- These branches are considered "floating" because they are pointing to a commit that has been replaced.
- `gits restack` will automatically rebase these floating branches onto the new `HEAD`.

**Arguments:**
- `--history-limit <n>`: Maximum first-parent history depth to scan while detecting floating branches. `0` disables the limit and scans the full history.
- `--autostash`: Allow the rebase loop to use Git autostash.
- `--no-autostash`: Disable Git autostash even if configured globally or for the repo.

**History limit resolution order:**
- CLI override: `--history-limit <n>`
- Repository config: `.git/gits.toml`
- Global config: the standard platform config directory as `gits/config.toml`
- Default: `100`

Example config:

```toml
[restack]
history_limit = 250
```

**Rebase autostash resolution order:**
- CLI override: `--autostash` or `--no-autostash`
- Repository config: `.git/gits.toml`
- Global config: the standard platform config directory as `gits/config.toml`
- Default: `false`

Example config:

```toml
[rebase]
autostash = true
```

**When to use it:** Use this after you've amended a commit or rebased a branch that has other branches building on top of it. Instead of manually rebasing each dependent branch, `gits restack` will find and fix them for you.

**ASCII-Art Visualization:**

```text
Before 'amend' on 'feature-A':
main -> [A1] -> (feature-A) -> [B1] -> (feature-B)

$ git commit --amend -m "A modified"

After 'amend' (feature-B is now floating on old A1):
main -> [A1'] -> (feature-A)
      \-> [A1] -> [B1] -> (feature-B)

$ gits restack

After gits restack:
main -> [A1'] -> (feature-A) -> [B1'] -> (feature-B)
```

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

### `pr`

**Description:** Manages pull requests for branches in the current stack.

**Usage:**

```bash
gits pr
gits pr open
gits pr edit
gits pr merge
gits pr status
gits pr review [--output <path>] [--copy] [--no-outdated] [--resolved] [--reviewer <login>] [--bots|--no-bots]
```

- `gits pr`: Create/update PRs for stack branches with upstreams.
- `gits pr open`: Open a PR URL in the default browser (if multiple, choose one).
- `gits pr edit`: Select a PR (if multiple), then edit title/body/labels/reviewers.
- `gits pr merge`: Select an open PR in the current stack and merge it only when review/check state is ready, or prompt/error with the blocking reasons.
- `gits pr status`: Show each stack PR's reviewer status, unresolved comments, and running/failed checks.
- `gits pr review`: Select an open PR in the current stack, fetch its review threads through `gh api graphql`, and render them as markdown.

`gits pr merge` automatically merges when the PR has no unresolved review comments, no outstanding review state, no running/failed checks, and GitHub reports the PR as mergeable. If issues remain but GitHub would still allow merging, `gits` prints the outstanding reviews/checks and asks for confirmation. If GitHub/repository rules block the merge, `gits` exits with a clear reason instead of attempting it.

`gits pr review` defaults to unresolved threads only, includes both human and bot comments, and keeps outdated comments unless you opt out.

**Arguments:**

- `-o, --output <path>`: Write the rendered markdown to a file.
- `--copy`: Copy the rendered markdown to the terminal clipboard using OSC 52.
- `--no-outdated`: Exclude outdated review threads.
- `--resolved`: Include resolved review threads.
- `--reviewer <login>`: Only include comments authored by the specified reviewer/login.
- `--bots`: Explicitly include bot comments/replies (this is the default).
- `--no-bots`: Restrict output to human-authored comments/replies.

**Formatting details:**

- Each top-level review thread is rendered as markdown with the file path and line number above the comment body.
- Replies are shown beneath the original comment in chronological order with one blank line between entries.
- Top-level review threads are separated by two blank lines.
- Outdated threads are labeled `OUTDATED` and include the original comment line number when rendered.

**Notes:**

- These commands require authenticated GitHub CLI (`gh auth status` must succeed).
- `gits pr status` is PR-focused and different from top-level `gits status` (which is for in-progress `move`/`commit` operations).

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

If a `gits commit`, `gits move`, or `gits restack` operation is interrupted (e.g., due to a merge conflict), use these commands to manage it:

- **`gits status`**: Shows the current state of an in-progress operation, including which branch is currently being rebased and which ones are remaining.
- **`gits continue`**: Resumes the operation after you've resolved conflicts. It handles the underlying `git rebase --continue` and then proceeds with the remaining branches in the stack.
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
