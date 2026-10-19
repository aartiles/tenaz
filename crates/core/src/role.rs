//! The six session roles, all played by the same model one after another.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Bootstrap,
    Builder,
    Reviewer,
    Split,
    Replan,
    FinalReviewer,
}

impl Role {
    pub const ALL: [Role; 6] = [
        Role::Bootstrap,
        Role::Builder,
        Role::Reviewer,
        Role::Split,
        Role::Replan,
        Role::FinalReviewer,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Bootstrap => "bootstrap",
            Role::Builder => "builder",
            Role::Reviewer => "reviewer",
            Role::Split => "split",
            Role::Replan => "replan",
            Role::FinalReviewer => "final-reviewer",
        }
    }

    /// `.tenaz/prompts/<role>.md`.
    pub fn prompt_path(self) -> String {
        format!(".tenaz/prompts/{}.md", self.as_str())
    }

    /// Roles whose sessions are expected to change files; the idle watchdog applies.
    pub fn edits_code(self) -> bool {
        matches!(self, Role::Builder | Role::Bootstrap)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Role::ALL
            .into_iter()
            .find(|r| r.as_str() == s)
            .ok_or_else(|| format!("unknown role {s}"))
    }
}
