# Kindra

Kindra is a CLI tool for managing **stacked git branches**. Its `kin` command automates the tedious parts of working with dependent branches, such as rebasing descendants after a commit or moving an entire stack of work to a new base.

## Key Features

- **Stacked Commits**: Automatically rebase all descendant branches when you commit in the middle of a stack.
- **Atomic Stack Moves**: Move a branch and all its descendants onto a new base branch in one pass using `--update-refs`.
- **Fork-Aware Reordering**: Edit branch parent relationships in your `$EDITOR`, including creating or preserving forks.
- **Smart Sync**: Rebase the current stack onto `main`/`master` in one pass using `--update-refs`, while skipping already-landed lower PRs.
- **Auto-Restack**: Automatically identify and repair "floating" branches that were based on an old version of the current branch (e.g., after an `amend` or `rebase`).
- **Interactive Navigation**: Quickly hop between branches in your stack with `up`, `down`, and `top` commands.

- **Visual Branch Splitting**: Assign branches to specific commits in a linear history using your favorite `$EDITOR`.
- **Atomic Pushes**: Push all branches in your stack simultaneously with `force-with-lease` safety.
- **Run Commands Across Stack**: Execute shell commands on each branch in your stack with `kin run`.
- **PR Workflow Helpers**: Create/update stack PRs, flatten stack PR bases to upstream, open PRs in your browser, edit PR metadata, inspect review/check status, export threaded review comments as markdown, and merge stack PRs with readiness checks.

## Installation

Kindra can be installed directly from GitHub:

```bash
cargo install --git https://github.com/Pajn/kindra.git kindra --bin kin
```

If you already use `cargo-binstall`, the git-based install works there too:

```bash
cargo binstall --git https://github.com/Pajn/kindra.git kindra
```

You can also install it from source:

```bash
# Clone the repository
git clone https://github.com/Pajn/kindra.git
cd kindra

# Build and install
cargo install --path .
```

## Quick Start

1. **Start a stack**: Create several branches, each building on the previous one.
2. **Make a change**: Checkout a branch in the middle of the stack and run `kin commit`.
3. **Watch the magic**: Kindra will automatically rebase all branches that depend on your change.
4. **Move the stack**: Ready to target a different feature? `kin move --onto main` to relocate the entire stack.
5. **Sync after merges**: If lower PRs landed, run `kin sync` to rebase the remaining stack onto latest `main`.
6. **Reorder the stack**: Need to reshuffle or fork branches? Run `kin reorder` and edit the parent map in your editor.
7. **Repair broken stacks**: Amended a commit and left dependent branches "floating"? Run `kin restack` to fix them.
8. **Manage PRs in stack**:
   - `kin pr` to create/update PRs
   - `kin pr open` to open a PR from the stack
   - `kin pr edit` to edit title/body/labels/reviewers
   - `kin pr flatten` to retarget all open stack PRs to the resolved upstream base branch on GitHub
   - `kin pr status` to inspect reviewers, unresolved comments, and failing/running checks
   - `kin pr review` to render PR review threads as markdown, optionally write them to a file, or copy them via OSC 52
   - `kin pr merge` to merge a stack PR only when reviews/checks are ready, or clearly explain/prompt when GitHub would still allow an override
9. **Run across stack**: `kin run -c "cargo test"` to run tests on each branch in the stack.

For a full list of commands and detailed examples, see the [CLI Reference](docs/cli_reference.md).

### `kin reorder` editor format

`kin reorder` opens a file with one row per branch:

```text
branch feature-c parent main
branch feature-a
branch feature-b
```

- `branch <name> parent <parent>` sets the branch parent explicitly.
- `branch <name>` means "make the branch on the previous line the parent".
- The first row must have an explicit parent, usually your upstream branch.
- Forks are created by repeating the same explicit parent on multiple rows.

Example fork:

```text
branch feature-c parent main
branch feature-a parent feature-c
branch feature-b parent feature-c
```

## Upstream Branch Selection

Commands that need an upstream/base branch (for example `sync`, `split`, `push`, `commit`, and `move`) resolve it in this order:

1. Repository override in `.git/kindra.toml`:

   ```toml
   upstream_branch = "branch-name"
   ```

2. `git config init.defaultBranch`
3. Built-in defaults: `main`, `master`, `trunk`
4. Remote fallbacks: `origin/<branch>`

## Managed Worktrees

Kindra now includes an opinionated `kin wt` workflow for managed git worktrees:

- `kin wt main` ensures a stable trunk worktree exists.
- `kin wt review [branch]` creates or reuses a fixed review worktree and repoints it safely.
- `kin wt temp [branch]` creates or reuses a branch-scoped disposable worktree.
- `kin wt list` shows all Kindra-managed worktrees and their current state.
- `kin wt path <target>` prints just the managed path for shell/editor integrations.
- `kin wt remove <target>` removes an explicit managed worktree with confirmation by default.
- `kin wt cleanup` removes merged or stale Kindra-managed temp worktrees.

By default Kindra stores managed worktrees under:

```text
.git/kindra-worktrees/
```

That keeps extra working trees out of the repo root while still making them easy to find and clean up.

### Examples

```bash
# Ensure a persistent trunk worktree exists
kin wt main

# Reuse a stable review workspace for the current branch
kin wt review

# Switch the review workspace to another branch
kin wt review feature/auth

# Create or reuse a temp worktree for a branch
kin wt temp feature/auth

# Use the resolved path in shell tooling
cd "$(kin wt path review)"

# Remove a single managed temp worktree
kin wt remove feature/auth

# Clean up merged temp worktrees
kin wt cleanup
```

### Worktree config

Managed worktrees use repo-local config in `.git/kindra.toml`:

```toml
[worktrees]
trunk = "main"

[worktrees.hooks]
on_create = []
on_checkout = []
on_remove = []

[worktrees.main]
path = ".git/kindra-worktrees/main"

[worktrees.review]
path = ".git/kindra-worktrees/review"

[worktrees.temp]
path_template = ".git/kindra-worktrees/temp/{branch}"
delete_merged = true
```

Notes:

- `main` is pinned to the configured trunk branch.
- `review` reuses a fixed path and refuses to discard local changes unless you confirm or pass `--force`.
- `cleanup` only targets Kindra-managed `temp` worktrees, never `main` or `review`.
- `kin wt path` is the script-friendly command: it prints only the resolved path on success.
- Use `branch:<name>` with `kin wt path` or `kin wt remove` to target a temp branch literally named `main` or `review`.
- Hooks run in the managed worktree directory and stop the action if they fail.

## Restack History Limit

`kin restack` bounds floating-branch discovery by default so very deep repositories do not pay for an unbounded first-parent scan.

Resolution order:

1. CLI override: `kin restack --history-limit <n>`
2. Repository config in `.git/kindra.toml`
3. Global config in the standard platform config directory as `kindra/config.toml`
4. Built-in default: `100`

Use `0` to disable the bound and scan the full first-parent history.

Example repository config:

```toml
[restack]
history_limit = 250
```

## Rebase Autostash

Commands that start a Git rebase (`commit`, `move`, `sync`, and `restack`) default to `--no-autostash` so dirty tracked changes do not get hidden implicitly.

Resolution order:

1. CLI override: `--autostash` or `--no-autostash`
2. Repository config in `.git/kindra.toml`
3. Global config in the standard platform config directory as `kindra/config.toml`
4. Built-in default: `false`

Example config:

```toml
[rebase]
autostash = true
```

## Benchmarking

Run the permanent Criterion benchmarks for stack navigation (`checkout top`, `co up`, `co down`) across two repository shapes:

- 5,000 commits on `main` + 10,000 noise branches
- 50,000 commits on `main` + 1,000 noise branches

```bash
cargo bench --bench checkout_top
```

## Why Kindra?

Traditional git workflows often involve large, monolithic Pull Requests or manual, error-prone rebasing when trying to keep multiple small, dependent PRs in sync. Kindra treats your branches as a **stack**, allowing you to focus on small, reviewable increments of code while it handles the plumbing.
