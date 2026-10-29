//! The six session roles, all played by the same model one after another.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Ordered as [`Role::ALL`], the order a run meets them in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

    /// What a steered session is asked to do before it stops: hand off for
    /// the roles that edit, write the one file it owns for the others. A
    /// reviewer told to write handoff notes writes them, and they are
    /// reverted.
    pub fn wrap_up(self) -> &'static str {
        match self {
            Role::Bootstrap | Role::Builder => {
                "Finish the smallest coherent step, run ./verify, write your handoff notes, and stop. The next session continues from the repository."
            }
            Role::Reviewer => {
                "Stop investigating: write your verdict file now from what you have found, and stop. Write no other file; a reviewer's notes are discarded."
            }
            Role::FinalReviewer => {
                "Stop investigating: write .tenaz/reviews/FINAL.md now from what you have found, with a card for each gap, and stop. Write no other file."
            }
            Role::Split | Role::Replan => "Write the task cards now from what you have, and stop.",
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_ordered_as_all_lists_them() {
        assert!(Role::ALL.windows(2).all(|w| w[0] < w[1]));
    }
}
