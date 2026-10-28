//! The fresh-context review of a task that passed `verify`.

use crate::briefing;
use crate::ledger::{JournalRecord, Verdict};
use crate::orchestrator::session::SessionCtx;
use crate::orchestrator::{RunError, Runner};
use crate::project::{only_minor_findings, verdict_in};
use crate::role::Role;
use crate::task::{TaskCard, TaskId};
use crate::verify::VerifyOutcome;

/// What a review session is given.
pub(super) struct ReviewInput<'a> {
    pub(super) card: &'a TaskCard,
    pub(super) card_text: &'a str,
    pub(super) base: &'a str,
    pub(super) attempt: u32,
    pub(super) all: &'a VerifyOutcome,
    pub(super) own: &'a VerifyOutcome,
    /// Why the orchestrator stopped the builder, when its work still passed.
    pub(super) stopped: Option<&'a str>,
}

/// The reviewer's verdict.
pub(super) enum Review {
    Accept,
    Changes(String),
    NoVerdict,
}

/// The task's diff for the reviewer, and the images it adds or replaces.
struct ReviewDiff {
    text: String,
    images: Vec<String>,
}

/// What a review left: its verdict and the review's text.
#[derive(Default)]
struct ReviewFile {
    verdict: Option<Verdict>,
    text: Option<String>,
}

/// Review sessions for one attempt: one retry after a review with no verdict.
const REVIEWS: u32 = 2;

/// Where the reviewer of `task` writes its review.
fn review_path(task: &TaskId) -> String {
    format!(".tenaz/reviews/{task}.md")
}

/// A `diff` over `limit` bytes: its `stat`, then the diff cut at `limit`
/// (on a character boundary), with a note to read the files instead.
fn clip_diff(diff: &str, stat: &str, limit: usize) -> String {
    let cut = diff.floor_char_boundary(limit);
    format!(
        "{}\n\n[Diff of {} bytes clipped. Read the changed files directly.]\n\n{}",
        stat.trim(),
        diff.len(),
        &diff[..cut]
    )
}

impl Runner {
    /// Fresh-context review of a task's diff. A reviewer that ends without
    /// a verdict (for example, stuck repeating itself in its reasoning) gets
    /// one fresh retry before `verify` alone decides.
    pub(super) async fn review_task(
        &mut self,
        input: &ReviewInput<'_>,
    ) -> Result<Review, RunError> {
        let task = &input.card.id;
        let diff = self.review_diff(input.base)?;
        self.project.remove(&review_path(task))?;
        let mut reviewed = ReviewFile::default();
        for review in 1..=REVIEWS {
            if review > 1 {
                self.log(&format!(
                    "  reviewer left no verdict for {task}; retrying the review"
                ));
            }
            reviewed = self.review_session(input, &diff, review).await?;
            if reviewed.verdict.is_some() {
                break;
            }
        }
        Ok(self.review_outcome(task, reviewed))
    }

    /// The diff since `base`, clipped to `diffLimitBytes` after its stat.
    fn review_diff(&self, base: &str) -> Result<ReviewDiff, RunError> {
        let mut diff = self.deps.vcs.diff(base, "HEAD", true)?;
        let images = briefing::changed_images(&diff);
        if diff.len() > self.params.diff_limit_bytes {
            let stat = self.deps.vcs.diff_stat(base, Some("HEAD"), false)?;
            diff = clip_diff(&diff, &stat, self.params.diff_limit_bytes);
        }
        Ok(ReviewDiff { text: diff, images })
    }

    /// One review session, number `review`: its verdict, journaled.
    async fn review_session(
        &mut self,
        input: &ReviewInput<'_>,
        diff: &ReviewDiff,
        review: u32,
    ) -> Result<ReviewFile, RunError> {
        let task = &input.card.id;
        let prompt = self.reviewer_prompt(input, diff)?;
        let s = self
            .session(
                Role::Reviewer,
                &prompt,
                &SessionCtx {
                    task: Some(task.clone()),
                    attempt: Some(input.attempt),
                    turns: Some(25),
                    context_tokens: None,
                },
            )
            .await?;
        let reviewed = self.read_review(task, &s.metrics.texts)?;
        self.journal(JournalRecord::Review {
            task: task.clone(),
            attempt: input.attempt,
            review,
            verdict: reviewed.verdict,
        });
        Ok(reviewed)
    }

    /// The reviewer's briefing: why the builder was stopped, its notes, the
    /// images to look at, the diff and the `verify` results.
    fn reviewer_prompt(
        &self,
        input: &ReviewInput<'_>,
        diff: &ReviewDiff,
    ) -> Result<String, RunError> {
        let task = &input.card.id;
        let mut extra = Vec::new();
        if let Some(stopped) = input.stopped {
            let note = format!(
                "The orchestrator stopped the builder session ({stopped}). The checks pass, but look for unfinished work."
            );
            extra.push(("Builder session".to_owned(), note));
        }
        extra.push((
            "Builder notes".to_owned(),
            self.project.note(task.as_str()).unwrap_or_default(),
        ));
        if !diff.images.is_empty() {
            extra.push((
                "Images this change adds or replaces".to_owned(),
                format!(
                    "{}\n\nLook at each one before you judge the task (see the role prompt).",
                    diff.images
                        .iter()
                        .map(|path| format!("- {path}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            ));
        }
        extra.push((
            "Diff since the task started".to_owned(),
            format!("```diff\n{}\n```", diff.text),
        ));
        extra.push((
            "Verify results".to_owned(),
            format!("{}\n\n{}", input.all.summary, input.own.summary),
        ));
        let fields = vec![
            ("Task".to_owned(), task.to_string()),
            ("Review file".to_owned(), review_path(task)),
        ];
        self.briefing(
            Role::Reviewer,
            fields,
            Some((&input.card.file, input.card_text)),
            extra,
            &[],
        )
    }

    /// The review file's verdict and text. A reviewer that gave its verdict
    /// in its last message (`texts`) but did not write it, or wrote it to
    /// the wrong path, is taken at its word.
    fn read_review(&self, task: &TaskId, texts: &[String]) -> Result<ReviewFile, RunError> {
        let path = review_path(task);
        let (verdict, text) = self.project.verdict(&path);
        if verdict.is_some() {
            return Ok(ReviewFile { verdict, text });
        }
        let Some((said, verdict)) = texts
            .iter()
            .rev()
            .find_map(|text| verdict_in(text).map(|verdict| (text, verdict)))
        else {
            return Ok(ReviewFile { verdict, text });
        };
        self.project.write(&path, said)?;
        self.log(&format!(
            "  {task}: the reviewer wrote no review file; its verdict is taken from its last message"
        ));
        Ok(ReviewFile {
            verdict: Some(verdict),
            text: Some(said.clone()),
        })
    }

    /// The review's outcome. Changes asked for with MINOR findings only are
    /// accepted.
    fn review_outcome(&self, task: &TaskId, ReviewFile { verdict, text }: ReviewFile) -> Review {
        match verdict {
            Some(Verdict::Accept) => Review::Accept,
            Some(Verdict::Changes) if text.as_deref().is_some_and(only_minor_findings) => {
                self.log(&format!(
                    "  {task}: the review asks for changes with MINOR findings only; accepted"
                ));
                Review::Accept
            }
            Some(Verdict::Changes) => Review::Changes(text.unwrap_or_default()),
            None => Review::NoVerdict,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clipped_diff_starts_with_its_stat_and_ends_at_the_limit() {
        let clipped = clip_diff("abcdefgh", " a.txt | 8 ++++++++\n", 3);
        assert_eq!(
            clipped,
            "a.txt | 8 ++++++++\n\n[Diff of 8 bytes clipped. Read the changed files directly.]\n\nabc"
        );
    }

    #[test]
    fn a_diff_is_clipped_before_a_character_cut_in_two() {
        // "é" is two bytes: a limit of 2 falls inside it.
        assert!(clip_diff("aé", "", 2).ends_with("\n\na"));
    }
}
