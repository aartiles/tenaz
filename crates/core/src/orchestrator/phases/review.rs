//! The fresh-context review of a task that passed `verify`.

use crate::briefing;
use crate::ledger::{JournalRecord, SessionRecord, Verdict};
use crate::orchestrator::session::{OUTPUT_LIMIT, SessionCtx};
use crate::orchestrator::{RunError, Runner};
use crate::project::{conflict_path, only_minor_findings, verdict_in};
use crate::role::Role;
use crate::session::SessionMetrics;
use crate::task::{TaskCard, TaskId};
use crate::util::CODE_BYTES_PER_TOKEN;
use crate::verify::VerifyOutcome;

use super::{PromptParts, clip_lines};

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
    /// No review gave a verdict: why each ended.
    NoVerdict(String),
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

/// What one review session left, and why it did not settle cleanly
/// (`SessionMetrics::stopped`) or its last reply was cut at the output
/// limit.
struct ReviewSession {
    file: ReviewFile,
    stopped: Option<String>,
}

/// Which review of an attempt a session is, and the note about the review
/// before it that its briefing starts with.
struct ReviewTry<'a> {
    number: u32,
    earlier: Option<&'a str>,
}

/// Why a review session ended short: `SessionMetrics::stopped`, or the
/// output limit when its harness settled after a reply cut there (fx exits
/// instead, and says so in `stopped`). `None` for a clean session.
fn review_stop(metrics: &SessionMetrics) -> Option<String> {
    metrics
        .stopped()
        .or_else(|| metrics.output_truncated().then(|| OUTPUT_LIMIT.to_owned()))
}

/// How a review that left no verdict ended, `stopped` as in [`retry_note`].
fn review_end(stopped: Option<&str>) -> &str {
    match stopped {
        None => "it stopped without writing one",
        Some("") => "it ended without saying why",
        Some(why) => why,
    }
}

/// Why a task is accepted unreviewed after reviews that ended as each of
/// `ended` says (`stopped` as in [`retry_note`]), in order.
fn unreviewed_reason(ended: &[Option<String>]) -> String {
    let each: Vec<String> = ended
        .iter()
        .zip(1..)
        .map(|(stopped, n)| format!("review {n}: {}", review_end(stopped.as_deref())))
        .collect();
    format!("no reviewer gave a verdict ({})", each.join("; "))
}

/// What a retried reviewer is told about the review before it, which left
/// no verdict and was `stopped` (`None` when it settled cleanly, empty when
/// it ended without saying why).
fn retry_note(stopped: Option<&str>) -> String {
    match stopped {
        None => "The review before this one stopped without writing its verdict. Write it to the review file before you stop.".to_owned(),
        Some("") => "The review before this one ended, without saying why, before it wrote its verdict. Write your verdict to the review file before anything else could end this session.".to_owned(),
        Some(why) => format!("The review before this one was stopped: {why}. Write your verdict to the review file before anything else could end this session."),
    }
}

/// Where the reviewer of `task` writes its review.
fn review_path(task: &TaskId) -> String {
    format!(".tenaz/reviews/{task}.md")
}

/// The most of a diff a reviewer with `budget` tokens is shown: `share` of
/// the budget at [`CODE_BYTES_PER_TOKEN`], and never over `ceiling` bytes.
fn diff_limit(budget: u64, share: f64, ceiling: usize) -> usize {
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a context budget times a share is far below 2^53, and a negative share saturates to 0 bytes"
    )]
    let bytes = (budget as f64 * share * CODE_BYTES_PER_TOKEN as f64).floor() as usize;
    bytes.min(ceiling)
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
        // How each review that left no verdict ended (`SessionMetrics::stopped`).
        let mut ended: Vec<Option<String>> = Vec::new();
        for review in 1..=REVIEWS {
            if review > 1 {
                self.log(&format!(
                    "  reviewer left no verdict for {task}; retrying the review"
                ));
            }
            let earlier = ended.last().map(|stopped| retry_note(stopped.as_deref()));
            let this_try = ReviewTry {
                number: review,
                earlier: earlier.as_deref(),
            };
            let session = self.review_session(input, &diff, &this_try).await?;
            reviewed = session.file;
            if reviewed.verdict.is_some() {
                break;
            }
            ended.push(session.stopped);
        }
        Ok(self.review_outcome(task, reviewed, &ended))
    }

    /// The diff since `base`, clipped to `diffShare` of the reviewer's
    /// budget, at most `diffLimitBytes`, after its stat (clipped to
    /// `statClipBytes`).
    fn review_diff(&self, base: &str) -> Result<ReviewDiff, RunError> {
        let mut diff = self.deps.vcs.diff(base, "HEAD", true)?;
        let images = briefing::changed_images(&diff);
        let limit = diff_limit(
            self.ledger.role_budget_tokens(Role::Reviewer, None),
            self.params.diff_share,
            self.params.diff_limit_bytes,
        );
        if diff.len() > limit {
            let stat = self.changed_files(base, Some("HEAD"), false)?;
            diff = clip_diff(&diff, &stat, limit);
        }
        Ok(ReviewDiff { text: diff, images })
    }

    /// One review session, told what the review before it left: its
    /// verdict, journaled, and why it did not settle cleanly.
    async fn review_session(
        &mut self,
        input: &ReviewInput<'_>,
        diff: &ReviewDiff,
        this_try: &ReviewTry<'_>,
    ) -> Result<ReviewSession, RunError> {
        let task = &input.card.id;
        let prompt = self.reviewer_prompt(input, diff, this_try.earlier)?;
        let s = self
            .session(
                Role::Reviewer,
                &prompt,
                &SessionCtx {
                    task: Some(task.clone()),
                    attempt: Some(input.attempt),
                    turns: Some(25),
                    context_tokens: None,
                    planner_before: None,
                },
            )
            .await?;
        let reviewed = self.read_review(task, &s.metrics.texts)?;
        self.note_stopped_with_verdict(&s.record, reviewed.verdict);
        self.journal(JournalRecord::Review {
            task: task.clone(),
            attempt: input.attempt,
            review: this_try.number,
            verdict: reviewed.verdict,
        });
        Ok(ReviewSession {
            file: reviewed,
            stopped: review_stop(&s.metrics),
        })
    }

    /// Says that a reviewer the orchestrator stopped had already given its
    /// verdict, which is used: otherwise it reads only as a failed session.
    fn note_stopped_with_verdict(&self, record: &SessionRecord, verdict: Option<Verdict>) {
        if let (Some(why), Some(verdict)) = (&record.abort_reason, verdict) {
            self.log(&format!(
                "  {}: {why} after writing its review; its verdict ({}) is used",
                record.label,
                verdict.as_str()
            ));
        }
    }

    /// The reviewer's briefing: what the earlier review left, if any,
    /// why the builder was stopped, its notes, the images to look at, the
    /// diff and the `verify` results.
    fn reviewer_prompt(
        &self,
        input: &ReviewInput<'_>,
        diff: &ReviewDiff,
        earlier: Option<&str>,
    ) -> Result<String, RunError> {
        let task = &input.card.id;
        let mut extra = Vec::new();
        if let Some(note) = earlier {
            extra.push(("Earlier review".to_owned(), note.to_owned()));
        }
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
        extra.extend(self.amends_section(input.card));
        extra.extend(self.passing_conflict_section(task));
        if !diff.images.is_empty() {
            let images: Vec<_> = diff.images.iter().map(|path| format!("- {path}")).collect();
            let images = clip_lines(&images.join("\n"), self.params.stat_clip_bytes);
            extra.push((
                "Images this change adds or replaces".to_owned(),
                format!(
                    "{images}\n\nLook at each one before you judge the task (see the role prompt)."
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
            PromptParts {
                fields,
                card: Some((&input.card.file, input.card_text)),
                extra,
                ..PromptParts::default()
            },
        )
    }

    /// The section that tells the reviewer a conflict file the builder left
    /// does not stand, since this attempt passes `verify` (ADR 0034); `None`
    /// without one.
    fn passing_conflict_section(&self, task: &TaskId) -> Option<(String, String)> {
        self.project.conflict_text(task.as_str())?;
        let path = conflict_path(task.as_str());
        Some((
            "Conflict file".to_owned(),
            format!(
                "The builder left `{path}`, but this attempt passes `verify`, so it does not stand for this attempt, and no card may amend the checks it names. Judge the work against the card and the spec as if the file were not there: what the card asks and the work does not do is a MAJOR finding, even though the builder left the file."
            ),
        ))
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

    /// The review's outcome, after reviews that `ended` without a verdict.
    /// Changes asked for with MINOR findings only are accepted.
    fn review_outcome(
        &self,
        task: &TaskId,
        ReviewFile { verdict, text }: ReviewFile,
        ended: &[Option<String>],
    ) -> Review {
        match verdict {
            Some(Verdict::Accept) => Review::Accept,
            Some(Verdict::Changes) if text.as_deref().is_some_and(only_minor_findings) => {
                self.log(&format!(
                    "  {task}: the review asks for changes with MINOR findings only; accepted"
                ));
                Review::Accept
            }
            Some(Verdict::Changes) => Review::Changes(text.unwrap_or_default()),
            None => Review::NoVerdict(unreviewed_reason(ended)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_review_cut_at_the_output_limit_ended_short_though_it_settled() {
        let clean = SessionMetrics {
            settled: true,
            exit_code: Some(0),
            ..SessionMetrics::default()
        };
        assert_eq!(review_stop(&clean), None);
        let cut = SessionMetrics {
            last_stop_reason: Some(crate::session::StopReason::MaxTokens),
            ..clean
        };
        assert_eq!(review_stop(&cut).as_deref(), Some(OUTPUT_LIMIT));
    }

    #[test]
    fn a_task_accepted_unreviewed_says_how_each_review_ended() {
        let ended = [
            Some("turn budget exceeded (25/25)".to_owned()),
            None,
            Some(String::new()),
        ];
        assert_eq!(
            unreviewed_reason(&ended),
            "no reviewer gave a verdict (review 1: turn budget exceeded (25/25); review 2: it stopped without writing one; review 3: it ended without saying why)"
        );
    }

    #[test]
    fn a_retry_after_a_stopped_review_is_told_why() {
        let note = retry_note(Some("turn budget exceeded (25/25)"));
        assert_eq!(
            note,
            "The review before this one was stopped: turn budget exceeded (25/25). Write your verdict to the review file before anything else could end this session."
        );
    }

    #[test]
    fn a_retry_after_a_review_that_ended_without_a_cause_says_so() {
        let note = retry_note(Some(""));
        assert!(note.contains("without saying why"), "{note}");
    }

    #[test]
    fn a_retry_after_a_review_that_settled_is_told_to_write_its_verdict() {
        let note = retry_note(None);
        assert!(
            note.contains("stopped without writing its verdict"),
            "{note}"
        );
        assert!(!note.contains("was stopped"), "{note}");
    }

    /// The Digger 3D run's budgets at `contextBudget` 0.65: Qwen3.8 on a
    /// 65536 window, and Bonsai on 131072.
    #[test]
    fn a_diff_takes_its_share_of_the_reviewer_budget_up_to_the_ceiling() {
        for (budget, share, limit) in [
            (42_598, 0.25, 31_948),
            (85_196, 0.25, 40_000),
            (42_598, 0.0, 0),
            (42_598, -1.0, 0),
        ] {
            assert_eq!(
                diff_limit(budget, share, 40_000),
                limit,
                "{budget} × {share}"
            );
        }
    }

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
