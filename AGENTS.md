# Quality Standards

### Testing & Coverage
- **Mandatory Integration Tests**: Every new feature or subcommand must include a corresponding integration test in the `tests/` directory.
- **Bug Regression Tests**: Any identified bug or edge case (e.g., panics, incorrect state) MUST be reproduced with a permanent test case before the fix is applied. Do not use temporary "repro" filenames; integrate them into the relevant test suite
- **Conflict Handling**: Commands that perform complex Git operations (like `move` or `split`) must be tested against rebase conflicts and incomplete states.
- **Operation State**: `tests/operation_state_tests.rs` pins the journal each operation saves when it pauses (golden files in `tests/fixtures/operation_state/golden/`, regenerated with `KIN_UPDATE_GOLDEN=1 cargo test --test operation_state_tests`; review the diff) and how journals written by released versions behave (`legacy/`, frozen — never regenerate them). In tests, name state files through `common::StateFile`/`rebase_state_file` and build `RebaseState` values with `common::rebase_state` and struct update syntax, not full literals.

### Linting & Formatting
- **Clippy**: Code must be Clippy-clean across all targets and features. Always run:
  ```bash
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  ```
- **Formatting**: Adhere to standard Rust formatting. Always run:
  ```bash
  cargo fmt --all
  ```

## Architecture & Design

### Modular Commands
- Subcommands should be implemented in individual files within `src/commands/`.
- The `src/main.rs` file should remain a thin entry point for CLI parsing and routing.

### Shared Logic
- Stack discovery and branch relationship logic must be centralized in `src/stack.rs`. Avoid duplicating Git graph traversal logic across different commands.
- Read Kindra config only through `src/config.rs`: repository config lives in the common Git directory (never `repo.path()`), and each module deserializes its own section with `ConfigFile::section`. Register new top-level keys in `KNOWN_REPO_KEYS`. Resolve the trunk only through `src/trunk.rs`.

### Safety & State
- Operations that modify multiple branches (like `move`) must persist their state to allow for `continue`/`abort` workflows.
- Always validate the exit status of system commands (e.g., `git checkout`, `git rebase`). Do not assume success.
- `src/operation_state.rs` owns what is in progress: the registry of persisted operation-state files (register new ones in `PersistedOperation`; never rename existing files), a pure `query` of the Kindra operation, native Git operation and override recovery facets, the explicit `reconcile` (needs `RepoLock`), and the `ensure_idle` gate. Every command calls the gate first, holding `RepoLock` and choosing an `Allow` set, before any overrides wrapper, `oplog::begin` or `save_state`, so a refused command persists nothing. Tell a native rebase from `git am` through `NativeOperation`, not by the presence of `rebase-apply/`.
- Every stash Kindra pushes, restores or drops goes through `src/set_aside.rs`: take a set-aside with `set_aside::take`/`take_tracked`, record it in the journal's `set_asides` before anything else can fail, and restore it with `set_aside::restore_all` (a journal's set-asides), `unwind` or `restore` (one set-aside), naming the lifecycle `Phase` — Completion, Abort, Unwind or NonResumable — which decides what a conflicted or failed restore does. `restore` leaves a restored entry on the stash stack: remove its record, save the journal, then call `set_aside::drop_restored` (even if that save failed, so the saved journal never re-applies it); `restore_all` and `unwind` do this themselves. A record whose entry is gone counts as restored. Do not run `git stash` elsewhere. A journal's gates treat any recorded set-aside as work still pending.
- Kindra owns every set-aside: pass `--no-autostash` to every `git rebase`, never `--autostash`. The autostash flags and config (`resolve_and_check_autostash`) are only the permission to set tracked changes aside; untracked files need none. Take an operation's set-aside before its journal is first saved (`rebase_utils::begin_replay` or `set_aside_working_tree` for branch replays and sync, after the commit for `kin commit`'s in-place paths), never later in the rebase loop, whose own set-aside is only for older journals that still ask for it (`legacy_autostash`). Set-asides include untracked files (`set_aside::take`); only `kin run` (and, for now, `kin split`) set aside tracked changes only (`take_tracked`). Tests must not keep mock `git` scripts, logs or trigger files in the working tree, which operations set aside: use the Git directory.
- The journal is versioned: `save_state` writes `{"version": N, "journal": {...}}` (`rebase_utils::JOURNAL_VERSION`) to the same file, so Kindra 1.1 and earlier, which expect the fields at the top level, refuse it instead of misreading it. Only `version` is fixed; `journal` may change shape with it. Bump the version whenever an older Kindra would misread what this one saves (a new field whose absence means something else, a changed meaning, a new value); every Kindra reads every older version and refuses a newer one with advice. A file without `version` is a flat journal saved by 1.1 or earlier and is converted when loaded; one with `version` but no `journal` is malformed. Never write the old fields alongside the new ones. Add a legacy fixture when a release ships a new shape.
- A journal's `operation` (`rebase_utils::Operation`) only names the command that paused, for status, refusals and messages; every command saves its own. Behaviour comes from `RebaseState::replay()`, derived from the label only for journals saved before `replay` existed.
- Named checkout persists source refs/OIDs and per-branch checkpoints in the worktree's `kindra_checkout_state.json` before creating branches. Continue retries creation at the recorded OID; abort retains created branches to protect edits. Keep hydration state in operation guards and override busy checks.
- `[hooks] after_pr` (`src/hooks.rs`) runs only after `kin pr`/`kin pr flatten` finished publishing, never on failure. Read the section before pushing so config errors change nothing; a hook failure exits non-zero without rolling back. The payload documented in `docs/cli_reference.md` is a stable interface: add fields, never change existing ones.
- Absorb preserves unnamed fork points with `update-ref` todo instructions placed after each autosquash group. Its temporary `refs/kindra/absorb/` anchors are recorded in the worktree's `new_base_map`; keep them through conflicts and delete only that operation's anchors when clearing recovery state.

## Development Workflow

1.  **Reproduce**: If fixing a bug, write a test that fails first.
2.  **Implement**: Apply the minimal surgical change required.
3.  **Verify**: Run the full test suite (`cargo test`) and check Clippy/Fmt.
4.  **Document**: Update this file or add comments for particularly complex Git graph operations.

### Local Override Lifecycle
- `src/overrides.rs` wraps whole operations while their existing `RepoLock` is held. Keep intermediate checkouts and rebases inside the wrapper; applying overlays between rebase steps can corrupt conflict resolutions.
- Override configuration is shared via the common Git directory, but snapshots and recovery phases belong to the individual worktree. Never restore the index from an override snapshot.
- New commands that change working-tree contents should use `with_suspended`; recovery commands must preserve suspended overlays until both Git and Kindra operation state are clear.
- `with_planned` keeps overlays applied. Every working-tree change inside it must follow a `prepare` call whose `Plan` names each commit to be checked out and each range to be replayed; `prepare` suspends if any of them changes an override path. Rebase-state commands get this from `run_rebase_loop` via `override_plan`; cover any checkout, reset, or rebase made before the loop yourself. Kindra stashes exclude untracked overlay files through `stash_pathspecs`.
- `kin overrides remove` disables automatic application per worktree. Recovery must honor persisted removal intent, and re-enabling must protect edits made to the original files while disabled.
- `kin overrides diff` compares configured on-disk files with HEAD through a temporary index and object database. Keep the real index, object database, and override state untouched; never run apply hooks for inspection.
