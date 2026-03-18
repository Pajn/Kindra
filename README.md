# gits

`gits` is a CLI tool designed to streamline the management of **stacked git branches**. It automates the tedious parts of working with dependencies between branches, such as rebasing descendants after a commit or moving an entire stack of work to a new base.

## Key Features

- **Stacked Commits**: Automatically rebase all descendant branches when you commit in the middle of a stack.
- **Atomic Stack Moves**: Move a branch and all its descendants onto a new base branch in one pass using `--update-refs`.
- **Fork-Aware Reordering**: Edit branch parent relationships in your `$EDITOR`, including creating or preserving forks.
- **Smart Sync**: Rebase the current stack onto `main`/`master` in one pass using `--update-refs`, while skipping already-landed lower PRs.
- **Auto-Restack**: Automatically identify and repair "floating" branches that were based on an old version of the current branch (e.g., after an `amend` or `rebase`).
- **Interactive Navigation**: Quickly hop between branches in your stack with `up`, `down`, and `top` commands.

- **Visual Branch Splitting**: Assign branches to specific commits in a linear history using your favorite `$EDITOR`.
- **Atomic Pushes**: Push all branches in your stack simultaneously with `force-with-lease` safety.
- **PR Workflow Helpers**: Create/update stack PRs, open PRs in your browser, edit PR metadata, inspect review/check status, export threaded review comments as markdown, and merge stack PRs with readiness checks.

## Installation

Currently, `gits` can be installed from source:

```bash
# Clone the repository
git clone https://github.com/Pajn/gits.git
cd gits

# Build and install
cargo install --path .
```

## Quick Start

1. **Start a stack**: Create several branches, each building on the previous one.
2. **Make a change**: Checkout a branch in the middle of the stack and run `gits commit`.
3. **Watch the magic**: `gits` will automatically rebase all branches that depend on your change.
4. **Move the stack**: Ready to target a different feature? `gits move --onto main` to relocate the entire stack.
5. **Sync after merges**: If lower PRs landed, run `gits sync` to rebase the remaining stack onto latest `main`.
6. **Reorder the stack**: Need to reshuffle or fork branches? Run `gits reorder` and edit the parent map in your editor.
7. **Repair broken stacks**: Amended a commit and left dependent branches "floating"? Run `gits restack` to fix them.
8. **Manage PRs in stack**:
   - `gits pr` to create/update PRs
   - `gits pr open` to open a PR from the stack
   - `gits pr edit` to edit title/body/labels/reviewers
   - `gits pr status` to inspect reviewers, unresolved comments, and failing/running checks
   - `gits pr review` to render PR review threads as markdown, optionally write them to a file, or copy them via OSC 52
   - `gits pr merge` to merge a stack PR only when reviews/checks are ready, or clearly explain/prompt when GitHub would still allow an override

For a full list of commands and detailed examples, see the [CLI Reference](docs/cli_reference.md).

### `gits reorder` editor format

`gits reorder` opens a file with one row per branch:

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

1. Repository override in `.git/gits.toml`:

   ```toml
   upstream_branch = "branch-name"
   ```

2. `git config init.defaultBranch`
3. Built-in defaults: `main`, `master`, `trunk`
4. Remote fallbacks: `origin/<branch>`

## Restack History Limit

`gits restack` bounds floating-branch discovery by default so very deep repositories do not pay for an unbounded first-parent scan.

Resolution order:

1. CLI override: `gits restack --history-limit <n>`
2. Repository config in `.git/gits.toml`
3. Global config in the standard platform config directory as `gits/config.toml`
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
2. Repository config in `.git/gits.toml`
3. Global config in the standard platform config directory as `gits/config.toml`
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

## Why gits?

Traditional git workflows often involve large, monolithic Pull Requests or manual, error-prone rebasing when trying to keep multiple small, dependent PRs in sync. `gits` treats your branches as a **stack**, allowing you to focus on small, reviewable increments of code while it handles the plumbing.
