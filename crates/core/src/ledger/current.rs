//! The ledger's record of the task being built: where it started, how its
//! attempts failed, and what the next attempt is told.

use serde::{Deserialize, Serialize};

use crate::task::TaskId;
use crate::util::clip;

/// A builder attempt the orchestrator stopped whose work still passed `verify`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoppedAttempt {
    pub attempt: u32,
    pub reason: String,
}

/// What `verify` said of a failed attempt, without why its session was
/// stopped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyFailure {
    /// The first attempt that failed with `text`.
    pub attempt: u32,
    pub text: String,
}

/// The task being built.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CurrentTask {
    pub task: TaskId,
    /// The commit the task started from.
    pub base: String,
    pub history: Vec<String>,
    pub failure: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stopped: Vec<StoppedAttempt>,
    /// The attempt whose passed work is being reviewed. Set until the
    /// review ends, so a run stopped during it (a pause, an engine that
    /// gave out) reviews that attempt again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_review: Option<u32>,
    /// The last `verify` failure, kept with the first attempt that had its
    /// text so a repeat can name it. Cleared when a review asks for changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_failure: Option<VerifyFailure>,
}

/// The bytes of an attempt's failure kept in the task's history.
const HISTORY_CLIP_BYTES: usize = 1500;

/// The bytes of a review the next attempt is told.
const REVIEW_CLIP_BYTES: usize = 3000;

impl CurrentTask {
    /// A task that starts from the commit `base`.
    pub fn new(task: TaskId, base: String) -> Self {
        Self {
            task,
            base,
            history: Vec::new(),
            failure: None,
            stopped: Vec::new(),
            in_review: None,
            verify_failure: None,
        }
    }

    /// Records `attempt` as failed `verify` with `verify_failure`, after
    /// the orchestrator `stopped` its session or not. The next attempt is
    /// told both. A failure that repeats the last one word for word is
    /// named in the history, not repeated: the split reads it once. The
    /// next attempt is told it is a repeat.
    pub fn fail(&mut self, attempt: u32, verify_failure: &str, stopped: Option<&str>) {
        let (entry, told) = match &self.verify_failure {
            Some(last) if last.text == verify_failure => (
                format!(
                    "The same verify failure as attempt {}, word for word.",
                    last.attempt
                ),
                format!("{verify_failure}\n\n{}", repeat_note(last.attempt)),
            ),
            _ => {
                self.verify_failure = Some(VerifyFailure {
                    attempt,
                    text: verify_failure.to_owned(),
                });
                (
                    clip(verify_failure, HISTORY_CLIP_BYTES),
                    verify_failure.to_owned(),
                )
            }
        };
        self.history.push(format!(
            "Attempt {attempt}: {}",
            with_stop_reason(&entry, stopped)
        ));
        self.failure = Some(with_stop_reason(&told, stopped));
    }

    /// Adds `note` to what the next attempt is told of the last failure.
    pub fn add_to_failure(&mut self, note: &str) {
        if let Some(failure) = &mut self.failure {
            failure.push_str("\n\n");
            failure.push_str(note);
        }
    }

    /// Records that the reviewer of `attempt` asked for changes in `text`.
    pub fn request_changes(&mut self, attempt: u32, text: &str) {
        self.failure = Some(format!(
            "The reviewer requested changes:\n{}",
            clip(text, REVIEW_CLIP_BYTES)
        ));
        self.history.push(format!(
            "Attempt {attempt}: review requested changes:\n{}",
            clip(text, HISTORY_CLIP_BYTES)
        ));
        self.verify_failure = None;
    }
}

/// What the next attempt is told after a failure that `verify` also
/// printed after attempt `first`. A check that only says `exit 1` reads the
/// same while work goes on, so the note does not claim more than that.
fn repeat_note(first: u32) -> String {
    format!(
        "`verify` printed this word for word after attempt {first} too. If it \
         reports a measurement, nothing done since moved it: do not try the same \
         approach again. Change it, or, if the card cannot be done as written, \
         say why in your notes with what you measured, and stop."
    )
}

/// An attempt's `failure` after the orchestrator `stopped` its session:
/// why comes first, so the next attempt knows the session did not end on
/// its own.
fn with_stop_reason(failure: &str, stopped: Option<&str>) -> String {
    match stopped {
        Some(stopped) => format!("The session was stopped: {stopped}.\n{failure}"),
        None => failure.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DARK: &str = "./verify --all fails after the attempt:\n24.0% dark";

    fn current() -> CurrentTask {
        CurrentTask::new(TaskId::parse("T001").unwrap(), "base".to_owned())
    }

    /// In the Lotion Pump run of 2026-10-28 three attempts of T016b ended
    /// with the same `verify` failure, and the split read it three times.
    #[test]
    fn a_repeated_verify_failure_names_the_attempt_that_has_its_text() {
        let mut task = current();
        task.fail(1, DARK, None);
        task.fail(2, DARK, Some("toolUse"));
        task.fail(3, DARK, None);
        let same = "The same verify failure as attempt 1, word for word.";
        assert_eq!(
            task.history,
            [
                format!("Attempt 1: {DARK}"),
                format!("Attempt 2: The session was stopped: toolUse.\n{same}"),
                format!("Attempt 3: {same}"),
            ]
        );
        let told = task.failure.unwrap();
        assert!(
            told.starts_with(DARK),
            "the next attempt is told it whole: {told}"
        );
    }

    /// The next attempt is told when the last one repeated an earlier
    /// failure, after why its session was stopped when it was.
    #[test]
    fn the_next_attempt_is_told_its_failure_repeats_an_earlier_one() {
        let note = "`verify` printed this word for word after attempt 1 too.";
        let mut task = current();
        task.fail(1, DARK, None);
        assert_eq!(task.failure.as_deref(), Some(DARK));
        task.fail(2, DARK, Some("toolUse"));
        let told = task.failure.clone().unwrap();
        assert!(
            told.starts_with(&format!(
                "The session was stopped: toolUse.\n{DARK}\n\n{note}"
            )),
            "{told}"
        );
        task.fail(3, "18.6% dark", None);
        assert_eq!(task.failure.as_deref(), Some("18.6% dark"));
    }

    #[test]
    fn a_different_verify_failure_is_not_named_as_a_repeat() {
        let mut task = current();
        task.fail(1, "24.0% dark", None);
        task.fail(2, "18.6% dark", None);
        assert_eq!(
            task.history,
            ["Attempt 1: 24.0% dark", "Attempt 2: 18.6% dark"]
        );
    }

    /// An attempt between two equal failures passed `verify` and was sent
    /// back by its review: the failure came back, it did not just repeat,
    /// so it is written in full again.
    #[test]
    fn a_verify_failure_after_a_review_is_written_in_full() {
        let mut task = current();
        task.fail(1, DARK, None);
        task.request_changes(2, "rename it");
        task.fail(3, DARK, None);
        assert_eq!(task.history[2], format!("Attempt 3: {DARK}"));
        assert_eq!(
            task.failure.as_deref(),
            Some(DARK),
            "the review is no longer the failure"
        );
    }

    #[test]
    fn the_history_clips_a_long_failure_that_the_next_attempt_is_told_whole() {
        let mut task = current();
        let long = "x".repeat(2000);
        task.fail(1, &long, None);
        assert!(
            task.history[0].ends_with("[... clipped 500 bytes]"),
            "{}",
            task.history[0]
        );
        assert_eq!(task.failure, Some(long));
    }

    /// "Word for word" holds past the clip: failures that differ only after
    /// the history's first 1500 bytes are not named as repeats.
    #[test]
    fn failures_that_differ_past_the_clip_are_not_named_as_repeats() {
        let mut task = current();
        let long = "x".repeat(2000);
        task.fail(1, &format!("{long}a"), None);
        task.fail(2, &format!("{long}b"), None);
        assert!(
            !task.history[1].contains("The same verify failure"),
            "{}",
            task.history[1]
        );
    }

    #[test]
    fn a_stopped_session_s_failure_says_why_it_was_stopped_first() {
        assert_eq!(
            with_stop_reason("verify fails", Some("loop")),
            "The session was stopped: loop.\nverify fails"
        );
        assert_eq!(with_stop_reason("verify fails", None), "verify fails");
    }

    /// A ledger written before `verify_failure` existed still loads.
    #[test]
    fn a_current_task_without_a_verify_failure_loads() {
        let json = r#"{"task":"T001","base":"b","history":[],"failure":null}"#;
        let task: CurrentTask = serde_json::from_str(json).unwrap();
        assert_eq!(
            task,
            CurrentTask::new(TaskId::parse("T001").unwrap(), "b".to_owned())
        );
    }
}
