//! Core library surface for `kindra`, the engine behind the `kin` CLI.
//!
//! The primary user-facing documentation lives in the project README and CLI
//! reference:
//!
//! - <https://github.com/Pajn/kindra>
//! - <https://github.com/Pajn/kindra/blob/main/docs/cli_reference.md>

pub mod commands;
pub mod editor;
pub mod gh;
pub mod rebase_utils;
pub mod repository;
pub mod stack;
pub mod worktree;

pub use commands::CheckoutSubcommand;
pub use repository::open_repo;
