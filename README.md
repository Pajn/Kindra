# gits

`gits` is a CLI tool designed to streamline the management of **stacked git branches**. It automates the tedious parts of working with dependencies between branches, such as rebasing descendants after a commit or moving an entire stack of work to a new base.

## Key Features

- **Stacked Commits**: Automatically rebase all descendant branches when you commit in the middle of a stack.
- **Atomic Stack Moves**: Move a branch and all its descendants onto a new base branch with a single command.
- **Interactive Navigation**: Quickly hop between branches in your stack with `up`, `down`, and `top` commands.
- **Visual Branch Splitting**: Assign branches to specific commits in a linear history using your favorite `$EDITOR`.
- **Atomic Pushes**: Push all branches in your stack simultaneously with `force-with-lease` safety.

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

For a full list of commands and detailed examples, see the [CLI Reference](docs/cli_reference.md).

## Why gits?

Traditional git workflows often involve large, monolithic Pull Requests or manual, error-prone rebasing when trying to keep multiple small, dependent PRs in sync. `gits` treats your branches as a **stack**, allowing you to focus on small, reviewable increments of code while it handles the plumbing.
