pub mod cleanup;
pub mod config;
pub mod git;
pub mod hooks;
pub mod metadata;
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
}

impl WorktreeRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Review => "review",
            Self::Temp => "temp",
        }
    }
}

impl Display for WorktreeRole {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
