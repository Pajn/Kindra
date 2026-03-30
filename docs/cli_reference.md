# Kindra CLI Reference

This document provides a detailed overview of the commands available in Kindra via the `kin` CLI and how to use them effectively for managing stacked git branches.

## Table of Contents

- [Core Concepts](#core-concepts)
- [Command Reference](#command-reference)
  - [commit](#commit)
  - [move](#move)
  - [reorder](#reorder)
  - [sync](#sync)
  - [restack](#restack)
  - [checkout (co)](#checkout-alias-co)
  - [worktree (wt)](#worktree-alias-wt)
  - [push](#push)
  - [run](#run)
  - [pr](#pr)
  - [split](#split)
  - [Status & Control (status, continue, abort)](#status--control)
  - [Shell Completions](#shell-completions)

---

## Core Concepts

Kindra is built around the idea of a **stack** of branches. A stack is a linear sequence of branches where each branch builds on top of the previous one, ultimately originating from a "base" branch (like `main` or `master`).

Kindra automatically identifies your stack by looking for local branches that are descendants of the merge base between your current branch and the base branch.

---

## Command Reference

### `commit`

**Description:** Commits changes and automatically rebases descendant branches in the affected stack in a single pass using `--update-refs` (Requires Git >= 2.38.0).

**Usage:**

```bash
kin commit [git-commit-args]
kin commit --on [<branch>] [git-commit-args]
kin commit --interactive [git-commit-args]
```

Any arguments you pass to `kin commit` (e.g., `-m "my message"`) are passed directly to `git commit`.

- `--on <branch>`: Commit onto another branch instead of the current one. The next token is consumed as the branch name.
- `--on=`: Open an interactive branch picker for the current stack.
- `--on`: Open the interactive branch picker only when `--on` is the final token.
- `--interactive`: Open an interactive commit picker showing all commits in the stack. Select a tip commit to amend, or an intermediate commit to fixup.
- `--autostash`: Allow the descendant rebase phase to use Git autostash.
- `--no-autostash`: Disable Git autostash even if configured globally or for the repo.

**Interactive mode behavior:**

When using `--interactive`:
- All commits across the stack are enumerated with their position (e.g., `feature-a 2/3`)
- Selecting a tip commit amends it and rebases any dependent branches
- Selecting an intermediate commit creates a fixup that is auto-squashed into the target
- Conflicts during the rebase enter the continue/abort workflow
- `--interactive` accepts and forwards trailing `git commit` args (including `-m` and pathspecs after `--`).

Examples:
- `kin commit --interactive -m "new subject"`
- `kin commit --interactive -- a3.txt`

Parser behavior:
- `kin commit --on feature-a -m "msg"`: valid (`feature-a` is the target branch).
- `kin commit --on -m "msg"`: invalid, because `--on` expects a branch unless used as the final token.
- Use `kin commit --on= -m "msg"` (or `kin commit --on` as the last token) for interactive selection.

When committing onto another branch, Kindra stashes non-staged files, switches to the target branch, commits, rebases dependents (unless you choose not to for an external stack), then returns to your original branch and unstages.

**When to use it:** Use this instead of `git commit` when you are working on a branch that has other branches building on top of it. It saves you from having to manually rebase each dependent branch.

**ASCII-Art Visualization:**

```text
Before commit on 'feature-A':
main -> [A1] -> (feature-A) -> [B1] -> (feature-B) -> [C1] -> (feature-C)

$ kin commit -m "update A"

After kin commit:
main -> [A1] -> [A2] -> (feature-A) -> [B1'] -> (feature-B) -> [C1'] -> (feature-C)
```

*(All descendant branches `feature-B` and `feature-C` are updated automatically.)*

---

### `move`

**Description:** Moves the current branch and all its descendants onto a new target branch in a single pass using `--update-refs` (Requires Git >= 2.38.0).

**Usage:**

```bash
kin move [--onto <target>] [--all] [--autostash|--no-autostash]
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

$ kin move --onto main

After kin move:
main -> [M1] -> [A1'] -> (feature-A) -> [B1'] -> (feature-B)
      \-> [D1] -> (develop)
```

---

### `reorder`

**Description:** Opens an editor so you can rewrite branch parent relationships for the current stack component, including forked stacks.

**Usage:**

```bash
kin reorder [--force] [--autostash|--no-autostash]
```

- `--force`: Continue even if a branch that needs rebasing is checked out in another worktree.
- `--autostash`: Allow the reorder rebase loop to use Git autostash.
- `--no-autostash`: Disable Git autostash even if configured globally or for the repo.

**Editor format:**

```text
branch feature-c parent main
branch feature-a
branch feature-b
```

- `branch <name> parent <parent>` sets an explicit parent.
- `branch <name>` means the branch listed immediately above becomes the parent.
- The first row must keep an explicit parent because there is no row above it.
- Forks are created by assigning the same explicit parent to multiple branches.

**What it does:**

- Loads the current connected stack component around your checked-out branch.
- Opens a temp file listing each branch and its current parent.
- Validates the edited graph: every branch must appear once, parents must be valid, cycles are rejected, and the result must stay connected to the upstream.
- Rebases branches in dependency order and restores your original checkout at the end.

**Recovery:**

- If a reorder started by Kindra stops on conflicts, resolve them and run `kin continue`.
- To abandon an in-progress reorder and restore the saved state, run `kin abort`.
- If you are in a plain native Git rebase with no saved Kindra state, use `git rebase --continue` or `git rebase --abort`.

**When to use it:** Use this when `move` is too narrow and you want to freely reshape a stack, such as rotating a linear stack, moving a branch across a fork, or turning a linear stack into siblings.

---

### `sync`

**Description:** Rebases the current stack onto the resolved upstream branch in a single rebase using `--update-refs` and automatically cleans up merged branches.

**Usage:**

```bash
kin sync [--force] [--no-delete] [--autostash|--no-autostash]
```

**Arguments:**
- `--force`: Force the sync even if branches in the stack are checked out in other worktrees.
- `--no-delete`: Do not automatically delete branches that have already been integrated into the upstream branch.
- `--autostash`: Allow the sync rebase to use Git autostash.
- `--no-autostash`: Disable Git autostash even if configured globally or for the repo.

**What it does:**

- Finds the resolved upstream/base branch using `find_upstream()` logic. This detection covers `.git/kindra.toml`, `init.defaultBranch`, common names like `main`/`master`/`trunk`, and remote-qualified bases.
- Finds the top branch in your current stack.
- Detects the first commit that still needs replaying (while handling lower PRs already landed via merge, rebase/cherry-pick, or squash).
- Checks out the top branch and runs one `git rebase --update-refs --onto <upstream> <old-base> <top>`.
- **Automatically deletes local branches** in the stack that are already merged into the upstream branch (unless `--no-delete` is used).
- If your current branch is deleted because it was merged, Kindra automatically switches you to the upstream branch.

**When to use it:** Use this after one or more lower PRs in your stack have already landed on the upstream branch, and you want to sync all remaining branches and clean up the merged ones in one pass.

**Conflict handling:** If a sync started by Kindra conflicts, resolve it and run `kin continue`, or abandon it with `kin abort`. If you are in a plain native Git rebase with no saved Kindra state, use `git rebase --continue` or `git rebase --abort`.

---

### `restack`

**Description:** Automatically identifies and repairs "floating" branches that were based on an old version of the current branch (e.g., after an `amend` or `rebase`).

**Usage:**

```bash
kin restack [--history-limit <n>] [--autostash|--no-autostash] [--pick]
```

**What it does:**
- Scans all local branches for those whose history includes a commit that "matches" the current `HEAD` but is not part of the current branch's ancestry. Patch-id fallback is limited to the current branch's private lineage so unrelated branches that only share upstream-equivalent patches are ignored.
- These branches are considered "floating" because they are pointing to a commit that has been replaced.
- `kin restack` will automatically rebase these floating branches onto the new `HEAD`.

**Arguments:**
- `--history-limit <n>`: Maximum first-parent history depth to scan while detecting floating branches. `0` disables the limit and scans the full history.
- `--autostash`: Allow the rebase loop to use Git autostash.
- `--no-autostash`: Disable Git autostash even if configured globally or for the repo.
- `--pick`: Show an interactive picker to select which branches to restack. Requires an interactive terminal. If no branches are selected, the command exits without performing any rebases.

**History limit resolution order:**
- CLI override: `--history-limit <n>`
- Repository config: `.git/kindra.toml`
- Global config: the standard platform config directory as `kindra/config.toml`
- Default: `100`

Example config:

```toml
[restack]
history_limit = 250
```

**Rebase autostash resolution order:**
- CLI override: `--autostash` or `--no-autostash`
- Repository config: `.git/kindra.toml`
- Global config: the standard platform config directory as `kindra/config.toml`
- Default: `false`

Example config:

```toml
[rebase]
autostash = true
```

**When to use it:** Use this after you've amended a commit or rebased a branch that has other branches building on top of it. Instead of manually rebasing each dependent branch, `kin restack` will find and fix them for you.

**ASCII-Art Visualization:**

```text
Before 'amend' on 'feature-A':
main -> [A1] -> (feature-A) -> [B1] -> (feature-B)

$ git commit --amend -m "A modified"

After 'amend' (feature-B is now floating on old A1):
main -> [A1'] -> (feature-A)
      \-> [A1] -> [B1] -> (feature-B)

$ kin restack

After kin restack:
main -> [A1'] -> (feature-A) -> [B1'] -> (feature-B)
```

---

### `checkout` (alias `co`)

**Description:** Provides an interactive interface to navigate branches in the stack.

**Usage:**

```bash
kin checkout [--all]
kin checkout [subcommand]
```

- `kin co`: Opens an interactive selection menu for branches in the current stack.
- `kin co --all`: Opens an interactive selection menu for all local branches.
- `kin co up`: Checkout the branch immediately "above" the current one in the stack.
- `kin co down`: Checkout the branch immediately "below" the current one in the stack.
- `kin co top`: Checkout the branch at the very top of the current stack.

**When to use it:** Use this for fast, ergonomic navigation without needing to remember branch names.

---

### `worktree` (alias `wt`)

**Description:** Manages Kindra-owned Git worktrees for trunk, review, and branch-scoped temporary work.

**Usage:**

```bash
kin worktree
kin wt list
kin wt main
kin wt review [<branch>] [--force]
kin wt temp [<branch>]
kin wt temp -b <new-branch> [<start-point>]
kin wt path <main|review|branch>
kin wt remove <main|review|branch> [--yes] [--force]
kin wt cleanup [--yes] [--force]
```

With no subcommand, `kin wt` behaves the same as `kin wt list`.

**Roles and defaults:**

- `main`: A persistent worktree pinned to the configured trunk branch.
- `review`: A persistent worktree at a fixed path that can be repointed to different branches.
- `temp`: Disposable branch-specific worktrees, one per branch.

By default, Kindra stores managed worktrees under:

```text
.git/kindra-worktrees/
  main
  review
  temp/<sanitized-branch-name>
```

For temp worktrees, Kindra sanitizes branch names for paths, so a branch like `feature/auth` becomes `.git/kindra-worktrees/temp/feature-auth`. If two different branch names would sanitize to the same path, `kin wt temp` fails instead of reusing the wrong directory.

The `kin wt temp` synopsis has two forms: without `-b`, the optional positional argument is treated as the branch name to open (or omitted to use the current branch); with `-b`, the positional argument is treated as the start point for the new branch.

**Subcommands:**

- `kin wt list`: Lists managed worktrees with role, branch, state, and path.
- `kin wt main`: Ensures the persistent `main` worktree exists and is checked out on the configured trunk branch. If the pinned path exists on some other branch, Kindra errors instead of switching it.
- `kin wt review [<branch>]`: Ensures the reusable `review` worktree exists. If no branch is provided, it uses the current branch. Reusing an existing review worktree on a different branch performs a checkout in place.
- `kin wt review --force <branch>`: Discards local changes in the review worktree before switching branches.
- `kin wt temp [<branch>]`: Ensures a temp worktree exists for the specified branch, or for the current branch if omitted.
- `kin wt temp -b <new-branch> [<start-point>]`: Creates a new local branch and checks it out in a temp worktree. If no start point is provided, Kindra uses the current branch.
- `kin wt path <target>`: Prints only the resolved path for `main`, `review`, or a temp worktree branch. This is intended for scripts and editor integrations.
- `kin wt remove <target>`: Removes a managed worktree. By default Kindra asks for confirmation; use `--yes` to skip the prompt.
- `kin wt remove --force <target>`: Forces `git worktree remove` when Git would otherwise refuse, such as for a dirty worktree.
- `kin wt cleanup`: Finds Kindra-managed temp worktrees that are merged into trunk or have stale metadata, prints the candidates, and removes the selected ones. It never removes `main` or `review`.

**State reporting:**

`kin wt list` reports a state column using these flags:

- `clean`: No special state applies.
- `current`: This row is the current worktree.
- `dirty`: The worktree has uncommitted changes.
- `merged`: A temp worktree branch has already been merged into trunk and is eligible for cleanup.
- `missing`: Kindra metadata exists but the worktree path no longer exists on disk.
- `stale-meta`: Kindra metadata does not match the live Git worktree state, or Kindra inferred a matching live worktree that is not recorded in metadata.

**Behavior notes:**

- `kin wt review` refuses to discard local changes when switching branches unless you confirm the prompt or pass `--force`.
- `kin wt main` and `kin wt temp` do not retarget an existing live worktree to another branch; they are pinned to their intended branch/path pairing.
- `kin wt path` fails if no managed worktree currently exists for that target.
- Removing or cleaning up a worktree can delete only metadata when the path is already gone and Git has pruned the live worktree entry.
- Worktree management requires a non-bare repository.

**Configuration:**

Managed worktrees are configured in `.git/kindra.toml`:

```toml
[worktrees]
root = ".git/kindra-worktrees"
trunk = "main"

[worktrees.hooks]
on_create = []
on_checkout = []
on_remove = []

[worktrees.main]
enabled = true
branch = "main"
path = ".git/kindra-worktrees/main"

[worktrees.review]
enabled = true
path = ".git/kindra-worktrees/review"
reuse = true
clean_before_switch = true

[worktrees.temp]
enabled = true
path_template = ".git/kindra-worktrees/temp/{branch}"
delete_merged = true
```

Configuration notes:

- `worktrees.trunk` defaults to Kindra's resolved upstream branch and falls back to `main` when no better upstream can be found.
- `worktrees.main.branch` defaults to `worktrees.trunk`.
- If `worktrees.trunk` resolves to a remote ref such as `origin/main`, Kindra bootstraps the local main worktree branch from that remote.
- `worktrees.temp.path_template` must include `{branch}`.
- `worktrees.review.clean_before_switch = false` skips Kindra's dirty-worktree cleanup prompt and lets plain `git checkout` decide whether the switch is possible.
- `worktrees.review.reuse = false` and `worktrees.main.allow_branch_switch = true` are not supported in the current implementation.
- Hook commands from `worktrees.hooks` and role-specific sections run in the managed worktree directory, and a failing hook aborts the action.

**When to use it:** Use this when you want stable, scriptable worktree locations for trunk and review, plus disposable branch worktrees that Kindra can list and clean up safely.

---

### `push`

**Description:** Pushes all branches in the current stack to their respective upstreams.

**Usage:**

```bash
kin push
```

This command performs an atomic push of all branches in the stack using `force-with-lease` to ensure safety.

**When to use it:** Use this when you've updated multiple branches in your stack (e.g., after a `kin commit` or `kin move`) and want to sync them all to the remote in one go.

---

### `run`

**Description:** Runs a shell command on each branch in the stack, starting from the base and moving toward the tips.

**Usage:**

```bash
kin run -c <command>
kin run --command <command>
kin run --continue-on-failure --command <command>
```

**Arguments:**
- `-c, --command <command>`: The shell command to run on each branch. Required.
- `--continue-on-failure`: If the command fails on a branch, continue to the next branch instead of stopping. By default, the command stops on the first failure.

**What it does:**
- Discovers stack branches from the current HEAD using the same logic as other Kindra commands.
- Sorts branches topologically from base to tips.
- For each branch:
  - Prints a header `=== Running on <branch> ===`
  - Checks out the branch
  - Executes the command via `sh -c`
  - Prints stdout and stderr
- Returns to the original branch when done.
- Prints a summary showing success/failure counts and any failed branches.

**When to use it:** Use this to run tests, linters, builds, or any other commands across all branches in your stack. For example, running `cargo test` on each branch to verify tests pass before creating PRs.

**Examples:**

```bash
# Run tests on all branches
kin run -c "cargo test"

# Run linter with continue-on-failure to see all results
kin run --continue-on-failure -c "cargo clippy"

# Run a custom command
kin run -c "echo 'Hello from $(git branch --show-current)'"
```

---

### `pr`

**Description:** Manages pull requests for branches in the current stack.

**Usage:**

```bash
kin pr [--no-push]
kin pr open
kin pr edit
kin pr flatten
kin pr merge
kin pr status
kin pr review [--output <path>] [--copy] [--no-outdated] [--resolved] [--reviewer <login>] [--bots|--no-bots]
```

- `kin pr`: Create/update PRs for stack branches with upstreams. By default it first checks whether open PR bases on GitHub still match the local stack order, flattens them to the resolved upstream base if needed, and pushes before running normal PR creation/update logic.
- `kin pr --no-push`: Skip that automatic flatten/push preflight and use the previous create/update behavior.
- `kin pr open`: Open a PR URL in the default browser (if multiple, choose one).
- `kin pr edit`: Select a PR (if multiple), then edit title/body/labels/reviewers.
- `kin pr flatten`: Retarget every open PR in the current stack to the resolved upstream base branch on GitHub (for example, `origin/main` normalizes to `main`).
- `kin pr merge`: Select an open PR in the current stack and merge it only when review/check state is ready, or prompt/error with the blocking reasons.
- `kin pr status`: Show each stack PR's reviewer status, unresolved comments, and running/failed checks. It also reports any interrupted `kin commit`, `kin move`, `kin reorder`, `kin sync`, or `kin restack` operation in the current repo and points you to `kin continue`/`kin abort` or native Git rebase commands when there is no saved Kindra state.
- `kin pr review`: Select an open PR in the current stack, fetch its review threads through `gh api graphql`, and render them as markdown.

`kin pr merge` automatically merges when the PR has no unresolved review comments, no outstanding review state, no running/failed checks, and GitHub reports the PR as mergeable. If issues remain but GitHub would still allow merging, Kindra prints the outstanding reviews/checks and asks for confirmation. If GitHub/repository rules block the merge, Kindra exits with a clear reason instead of attempting it.

`kin pr flatten` only updates PR base branches on GitHub. It does not modify local git refs, stack relationships, PR titles, or PR bodies.

`kin pr review` defaults to unresolved threads only, includes both human and bot comments, and keeps outdated comments unless you opt out.

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
- Both `kin status` and `kin pr status` report interrupted `kin commit`, `kin move`, `kin reorder`, `kin sync`, and `kin restack` operations.
- When a saved Kindra state exists, continue with `kin continue` or clean up with `kin abort`. If there is no saved Kindra state and Git itself is mid-rebase, use `git rebase --continue` or `git rebase --abort`.

---

### `split`

**Description:** Opens your `$EDITOR` to visually manage branch assignments for a series of commits.

**Usage:**

```bash
kin split
```

It generates a list of commits and branches. You can move the `branch <name>` lines to reassign branches to different commits, or add/remove them to create/delete branches.

**When to use it:** Use this when you've made a long series of commits on a single branch and want to "split" them into multiple separate, dependent branches for easier review.

**ASCII-Art Visualization:**

```text
Before split (one branch, multiple commits):
main -> [C1] -> [C2] -> [C3] -> (my-feature)

$ kin split
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

If a `kin commit`, `kin move`, `kin reorder`, `kin sync`, or `kin restack` operation is interrupted (e.g., due to a merge conflict), both `kin status` and `kin pr status` report the interrupted operation and how to recover:

- **`kin status`**: Shows the current state of the interrupted operation, including which branch is currently being rebased and which ones are remaining.
- **`kin continue`**: Resumes the operation after you've resolved conflicts. It handles the underlying `git rebase --continue` and then proceeds with the remaining branches in the stack.
- **`kin abort`**: Cancels the current operation and cleans up the state.
- If there is no saved Kindra state and Git itself is in the middle of a native rebase, use `git rebase --continue` or `git rebase --abort`.

---

### Shell Completions

**Description:** Generates shell completion scripts for various shells.

**Usage:**

```bash
kin completions <shell>
```

Supported shells: `bash`, `zsh`, `fish`, `powershell`, `elvish`, `nu`.

**Installation Example (Zsh):**

```bash
mkdir -p ~/.zsh/completions
kin completions zsh > ~/.zsh/completions/_kin
fpath=(~/.zsh/completions $fpath)
autoload -Uz compinit && compinit
```
