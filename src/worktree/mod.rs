pub mod cleanup;
pub mod config;
pub mod git;
pub mod hooks;
pub mod path_resolver;
pub mod roles;
pub mod ui;

use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeRole {
    Main,
    Review,
    Temp,
    /// Existing worktrees eligible for cleanup, with no creation location.
    Cleanup,
}

impl WorktreeRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Review => "review",
            Self::Temp => "temp",
            Self::Cleanup => "cleanup",
        }
    }
}

impl Display for WorktreeRole {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
