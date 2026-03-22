# Quality Standards

### Testing & Coverage
- **Mandatory Integration Tests**: Every new feature or subcommand must include a corresponding integration test in the `tests/` directory.
- **Bug Regression Tests**: Any identified bug or edge case (e.g., panics, incorrect state) MUST be reproduced with a permanent test case before the fix is applied. Do not use temporary "repro" filenames; integrate them into the relevant test suite
- **Conflict Handling**: Commands that perform complex Git operations (like `move` or `split`) must be tested against rebase conflicts and incomplete states.

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

### Safety & State
- Operations that modify multiple branches (like `move`) must persist their state to allow for `continue`/`abort` workflows.
- Always validate the exit status of system commands (e.g., `git checkout`, `git rebase`). Do not assume success.

## Development Workflow

1.  **Reproduce**: If fixing a bug, write a test that fails first.
2.  **Implement**: Apply the minimal surgical change required.
3.  **Verify**: Run the full test suite (`cargo test`) and check Clippy/Fmt.
4.  **Document**: Update this file or add comments for particularly complex Git graph operations.
