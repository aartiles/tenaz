//! Task cards: one Markdown file per task under `.tenaz/tasks/`, with a
//! frontmatter the orchestrator partly owns (`status`, `attempts`).

pub mod frontmatter;

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use self::frontmatter::{Value, parse_frontmatter};

/// `T007`, or a split child such as `T007a` or `T007aa`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(String);

impl TaskId {
    pub fn parse(text: &str) -> Result<Self, CardError> {
        let digits = text.strip_prefix('T').unwrap_or("");
        let (number, suffix) =
            digits.split_at(digits.bytes().take_while(u8::is_ascii_digit).count());
        if number.len() == 3 && suffix.bytes().all(|b| b.is_ascii_lowercase()) {
            Ok(Self(text.to_owned()))
        } else {
            Err(CardError::BadId(text.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `T007` is 0, `T007a` is 1, `T007aa` is 2.
    pub fn split_depth(&self) -> usize {
        self.0
            .bytes()
            .rev()
            .take_while(u8::is_ascii_lowercase)
            .count()
    }

    /// `T007.md`.
    pub fn file_name(&self) -> String {
        format!("{}.md", self.0)
    }

    /// `.tenaz/tasks/T007.md`.
    pub fn card_path(&self) -> String {
        format!(".tenaz/tasks/{}", self.file_name())
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for TaskId {
    type Err = CardError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// The orchestrator-owned lifecycle of a card.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Todo,
    Active,
    Done,
    Blocked,
    Split,
    Dropped,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Todo => "todo",
            TaskStatus::Active => "active",
            TaskStatus::Done => "done",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Split => "split",
            TaskStatus::Dropped => "dropped",
        }
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskStatus {
    type Err = CardError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "todo" => Ok(TaskStatus::Todo),
            "active" => Ok(TaskStatus::Active),
            "done" => Ok(TaskStatus::Done),
            "blocked" => Ok(TaskStatus::Blocked),
            "split" => Ok(TaskStatus::Split),
            "dropped" => Ok(TaskStatus::Dropped),
            other => Err(CardError::BadStatus(other.to_owned())),
        }
    }
}

/// Per-task session budget from the card (`budget: {turns: 40, tokens: 60000}`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    pub turns: Option<u32>,
    pub tokens: Option<u64>,
}

/// A parsed task card.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskCard {
    pub id: TaskId,
    pub title: String,
    pub status: TaskStatus,
    pub requires: Vec<String>,
    pub depends: Vec<TaskId>,
    pub files: Vec<String>,
    pub parent: Option<TaskId>,
    pub attempts: u32,
    pub budget: Budget,
    /// Everything after the frontmatter.
    pub body: String,
    /// `.tenaz/tasks/T007.md`.
    pub file: String,
}

/// A card that could not be read.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CardError {
    #[error(transparent)]
    Frontmatter(#[from] frontmatter::FrontmatterError),
    #[error("id \"{0}\" must look like T007 or T007a")]
    BadId(String),
    #[error("file name must be {0}")]
    FileName(String),
    #[error("unknown status {0}")]
    BadStatus(String),
}

impl TaskCard {
    /// Parses the card stored as `.tenaz/tasks/<file_name>`.
    pub fn parse(file_name: &str, text: &str) -> Result<Self, CardError> {
        let fm = parse_frontmatter(text)?;
        let text_of = |key: &str| fm.get(key).and_then(Value::to_text).unwrap_or_default();
        let id = TaskId::parse(&text_of("id"))?;
        if file_name != id.file_name() {
            return Err(CardError::FileName(id.file_name()));
        }
        let status = match fm.get("status") {
            None => TaskStatus::Todo,
            Some(value) => value.to_text().unwrap_or_default().parse()?,
        };
        let list_of = |key: &str| fm.get(key).map(Value::to_list).unwrap_or_default();
        let mut depends = list_of("depends")
            .iter()
            .map(|d| TaskId::parse(d))
            .collect::<Result<Vec<_>, _>>()?;
        // A task id under `requires` is a dependency written in the wrong list.
        let (task_ids, requires): (Vec<String>, Vec<String>) = expand_ranges(list_of("requires"))
            .into_iter()
            .partition(|r| TaskId::parse(r).is_ok());
        for id in task_ids.iter().filter_map(|r| TaskId::parse(r).ok()) {
            if !depends.contains(&id) {
                depends.push(id);
            }
        }
        let parent = match fm.get("parent").and_then(Value::to_text) {
            Some(p) if !p.is_empty() => Some(TaskId::parse(&p)?),
            _ => None,
        };
        let budget = match fm.get("budget") {
            Some(map @ Value::Map(_)) => Budget {
                turns: map
                    .get("turns")
                    .and_then(Value::as_number)
                    .and_then(whole_u32),
                tokens: map
                    .get("tokens")
                    .and_then(Value::as_number)
                    .and_then(whole_u64),
            },
            _ => Budget::default(),
        };
        let attempts = fm
            .get("attempts")
            .and_then(Value::as_number)
            .and_then(whole_u32)
            .unwrap_or(0);
        Ok(Self {
            file: format!(".tenaz/tasks/{file_name}"),
            id,
            title: text_of("title"),
            status,
            requires,
            depends,
            files: list_of("files"),
            parent,
            attempts,
            budget,
            body: fm.body,
        })
    }

    /// Whether `text` describes a card whose status is `done` (such a card is
    /// history and nobody may rewrite it).
    pub fn text_is_done(text: &str) -> bool {
        text.lines().any(|line| {
            line.strip_prefix("status:")
                .is_some_and(|rest| rest.trim() == "done")
        })
    }
}

/// A non-negative whole number, or nothing: a card value such as `-3` or `2.5`
/// is ignored rather than guessed.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn whole_u32(n: f64) -> Option<u32> {
    (n >= 0.0 && n.fract() == 0.0 && n <= f64::from(u32::MAX)).then_some(n as u32)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn whole_u64(n: f64) -> Option<u64> {
    (n >= 0.0 && n.fract() == 0.0 && n <= u64::MAX as f64).then_some(n as u64)
}

/// Writes orchestrator-owned fields into a card's text.
pub fn set_card_fields(text: &str, fields: &[(&str, String)]) -> String {
    frontmatter::set_fields(text, fields)
}

/// Requirement ranges written as `R1-R9` (or with an en dash) become the
/// ids they span; anything else stays as written. A range of more than 500
/// ids is left alone.
fn expand_ranges(items: Vec<String>) -> Vec<String> {
    let number = |s: &str| s.trim().strip_prefix('R')?.parse::<u32>().ok();
    let mut out = Vec::new();
    for item in items {
        let span = item
            .split_once(['-', '\u{2013}'])
            .and_then(|(a, b)| Some((number(a)?, number(b)?)))
            .filter(|(a, b)| a <= b && b - a < 500);
        match span {
            Some((a, b)) => out.extend((a..=b).map(|n| format!("R{n}"))),
            None => out.push(item),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: &str = "---\nid: T007\ntitle: Import CSV\nstatus: todo\nrequires: [R4, R9]\ndepends: [T003]\nfiles: [src/import.js, verify]\nbudget: {turns: 30, tokens: 50000}\nattempts: 2\n---\n## Goal\nOne line.\n";

    #[test]
    fn parses_every_field() {
        let card = TaskCard::parse("T007.md", CARD).unwrap();
        assert_eq!(card.id.as_str(), "T007");
        assert_eq!(card.title, "Import CSV");
        assert_eq!(card.status, TaskStatus::Todo);
        assert_eq!(card.requires, vec!["R4", "R9"]);
        let misplaced = TaskCard::parse(
            "T007.md",
            "---\nid: T007\nrequires: [R6, T006, T002]\ndepends: [T006]\n---\n",
        )
        .unwrap();
        assert_eq!(misplaced.requires, ["R6"]);
        assert_eq!(
            misplaced.depends,
            [
                TaskId::parse("T006").unwrap(),
                TaskId::parse("T002").unwrap()
            ]
        );
        assert_eq!(
            expand_ranges(vec![
                "R1-R3".into(),
                "R7".into(),
                "R5\u{2013}R6".into(),
                "R9-R2".into()
            ]),
            ["R1", "R2", "R3", "R7", "R5", "R6", "R9-R2"]
        );
        assert_eq!(card.depends, vec![TaskId::parse("T003").unwrap()]);
        assert_eq!(card.files, vec!["src/import.js", "verify"]);
        assert_eq!(
            card.budget,
            Budget {
                turns: Some(30),
                tokens: Some(50000)
            }
        );
        assert_eq!(card.attempts, 2);
        assert_eq!(card.parent, None);
        assert_eq!(card.body, "## Goal\nOne line.\n");
        assert_eq!(card.file, ".tenaz/tasks/T007.md");
    }

    #[test]
    fn minimal_card_defaults() {
        let card = TaskCard::parse("T001.md", "---\nid: T001\n---\n").unwrap();
        assert_eq!(card.status, TaskStatus::Todo);
        assert_eq!(card.depends, Vec::<TaskId>::new());
        assert_eq!(card.budget, Budget::default());
        assert_eq!(card.attempts, 0);
    }

    #[test]
    fn split_children_and_depth() {
        let child = TaskCard::parse("T007a.md", "---\nid: T007a\nparent: T007\n---\n").unwrap();
        assert_eq!(child.parent.as_ref().map(TaskId::as_str), Some("T007"));
        assert_eq!(child.id.split_depth(), 1);
        assert_eq!(TaskId::parse("T007aa").unwrap().split_depth(), 2);
        assert_eq!(TaskId::parse("T007").unwrap().split_depth(), 0);
    }

    #[test]
    fn rejects_bad_ids_names_and_statuses() {
        assert_eq!(
            TaskCard::parse("T7.md", "---\nid: T7\n---\n").unwrap_err(),
            CardError::BadId("T7".into())
        );
        assert_eq!(
            TaskId::parse("T007A"),
            Err(CardError::BadId("T007A".into()))
        );
        assert_eq!(
            TaskCard::parse("other.md", "---\nid: T007\n---\n").unwrap_err(),
            CardError::FileName("T007.md".into())
        );
        assert_eq!(
            TaskCard::parse("T007.md", "---\nid: T007\nstatus: maybe\n---\n").unwrap_err(),
            CardError::BadStatus("maybe".into())
        );
        assert!(matches!(
            TaskCard::parse("T007.md", "no frontmatter"),
            Err(CardError::Frontmatter(_))
        ));
    }

    #[test]
    fn done_detection_reads_the_frontmatter_line() {
        assert!(TaskCard::text_is_done("---\nid: T1\nstatus: done \n---\n"));
        assert!(!TaskCard::text_is_done("---\nid: T1\nstatus: todo\n---\n"));
        assert!(!TaskCard::text_is_done(
            "---\nid: T1\nstatus: doneish\n---\n"
        ));
    }

    #[test]
    fn set_card_fields_updates_status_and_attempts() {
        let out = set_card_fields(CARD, &[("status", "done".into()), ("attempts", "3".into())]);
        let card = TaskCard::parse("T007.md", &out).unwrap();
        assert_eq!(card.status, TaskStatus::Done);
        assert_eq!(card.attempts, 3);
        assert_eq!(card.body, "## Goal\nOne line.\n");
    }
}
