//! What the bootstrap makes of a failed attempt: the problems the next
//! session is told, whether the attempt counts, and why the run blocks.
//! The runner validates, journals and logs; this module only decides.

use crate::ledger::{BootstrapStep, HarnessKind};
use crate::ownership::reverted_note;
use crate::role::Role;
use crate::session::SessionMetrics;

/// The problems of an attempt: the `errors` found in its files, after what
/// went wrong in the session itself, so the next session does not repeat
/// it. A session that did its step has nothing to add.
pub(crate) fn diagnose(
    errors: Vec<String>,
    metrics: &SessionMetrics,
    reverted: &[String],
) -> Vec<String> {
    if errors.is_empty() {
        return errors;
    }
    let mut problems = Vec::new();
    // A session whose every tool call failed did not get to the task: say
    // so first, for the next session and for whoever reads the block.
    if metrics.every_tool_call_failed() {
        problems.push(format!(
            "every tool call in the session failed ({} of {}), so it could not read or \
             write the project; the first: {}",
            metrics.tool_errors,
            metrics.tool_calls,
            first_tool_error(metrics)
        ));
    }
    // A reply cut at the output limit left no work: say why, so the next
    // session does not reason the same way again.
    if metrics.output_truncated()
        && let Some(reason) = &metrics.abort_reason
    {
        problems.push(format!("the session was stopped: {reason}"));
    }
    // A file the session wrote where it may not was reverted after it. The
    // next session otherwise sees only the failure the revert caused (verify
    // calling a file that is gone) and repeats it.
    if !reverted.is_empty() {
        problems.push(reverted_note(Role::Bootstrap, reverted));
    }
    // A file written outside the project is a file the validation finds
    // missing: say where it went, so the next session (and whoever reads
    // the block) knows the files were written, only in the wrong place.
    if !metrics.outside_writes.is_empty() {
        problems.push(format!(
            "the session wrote files outside the project, where the orchestrator does not \
             look: {}; write them in the project, at the paths named here",
            metrics.outside_writes.join(", ")
        ));
    }
    problems.extend(errors);
    problems
}

/// When an attempt that left `after` problems made progress over the one
/// before, the count that one left: such an attempt is not counted, up to
/// `max` times, as many as the counted attempts. `uncounted` such came
/// before it; the first attempt has nothing to make progress over.
pub(crate) fn progress(
    before: Option<usize>,
    after: usize,
    uncounted: u32,
    max: u32,
) -> Option<usize> {
    before.filter(|&before| after < before && uncounted < max)
}

/// What the log adds to a step that was not accepted.
pub(crate) fn tool_failure_note(metrics: &SessionMetrics) -> String {
    if metrics.every_tool_call_failed() {
        format!(
            "; all {} tool calls failed, the first: {}",
            metrics.tool_calls,
            first_tool_error(metrics)
        )
    } else {
        String::new()
    }
}

/// The counted attempts at a bootstrap step, and how many of them failed
/// every tool call.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Attempts {
    pub counted: u32,
    pub tool_failures: u32,
}

/// Why the run blocks after the counted `attempts` at `step`, with the
/// problems the last one left. When every attempt's tool calls all failed,
/// the model does not work with `harness`: say so.
pub(crate) fn exhausted(
    step: BootstrapStep,
    attempts: Attempts,
    harness: HarnessKind,
    last_errors: Option<&[String]>,
) -> String {
    let errors = last_errors.unwrap_or_default().join("; ");
    let hint = if attempts.tool_failures >= attempts.counted {
        format!(
            " (in every attempt all of the model's tool calls failed: the model does not \
             work with {harness}'s tools; try another model or the other harness)"
        )
    } else {
        String::new()
    };
    let counted = attempts.counted;
    format!("bootstrap failed {counted} times at step {step}{hint}: {errors}")
}

/// The first failed tool call; a record from before the field existed has none.
fn first_tool_error(metrics: &SessionMetrics) -> &str {
    metrics.first_tool_error.as_deref().unwrap_or("unknown")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::StopReason;

    const ERROR: &str = "./verify --all does not pass";
    /// As the watchdog writes it: the tool's name, then what it said.
    const FIRST_TOOL_ERROR: &str = "bash: command not found: rg";

    fn errors() -> Vec<String> {
        vec![ERROR.to_owned()]
    }

    fn tool_calls(failed: u32) -> SessionMetrics {
        SessionMetrics {
            tool_calls: 3,
            tool_errors: failed,
            first_tool_error: (failed > 0).then(|| FIRST_TOOL_ERROR.to_owned()),
            ..SessionMetrics::default()
        }
    }

    fn tools_failed() -> SessionMetrics {
        tool_calls(3)
    }

    fn tools_failed_message() -> String {
        tools_failed_with(FIRST_TOOL_ERROR)
    }

    fn tools_failed_with(first: &str) -> String {
        format!(
            "every tool call in the session failed (3 of 3), so it could not read or write \
             the project; the first: {first}"
        )
    }

    fn cut_at_output_limit(reason: Option<&str>) -> SessionMetrics {
        SessionMetrics {
            last_stop_reason: Some(StopReason::MaxTokens),
            abort_reason: reason.map(str::to_owned),
            ..SessionMetrics::default()
        }
    }

    fn reverted() -> Vec<String> {
        vec![".tenaz/brief.md".to_owned()]
    }

    fn attempts(counted: u32, tool_failures: u32) -> Attempts {
        Attempts {
            counted,
            tool_failures,
        }
    }

    #[test]
    fn a_session_that_did_its_step_has_no_problems() {
        assert_eq!(
            diagnose(Vec::new(), &tools_failed(), &reverted()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_clean_session_keeps_the_validation_errors_as_they_are() {
        assert_eq!(
            diagnose(errors(), &SessionMetrics::default(), &[]),
            errors()
        );
    }

    #[test]
    fn failed_tool_calls_are_named_before_the_errors() {
        assert_eq!(
            diagnose(errors(), &tools_failed(), &[]),
            [tools_failed_message(), ERROR.to_owned()]
        );
    }

    #[test]
    fn a_session_where_only_some_tool_calls_failed_adds_nothing() {
        assert_eq!(diagnose(errors(), &tool_calls(1), &[]), errors());
    }

    #[test]
    fn failed_tool_calls_without_a_recorded_first_error_say_unknown() {
        let metrics = SessionMetrics {
            first_tool_error: None,
            ..tools_failed()
        };
        assert_eq!(
            diagnose(errors(), &metrics, &[]),
            [tools_failed_with("unknown"), ERROR.to_owned()]
        );
    }

    #[test]
    fn a_reply_cut_at_the_output_limit_says_why_the_session_stopped() {
        let metrics = cut_at_output_limit(Some("reply reached the output limit"));
        assert_eq!(
            diagnose(errors(), &metrics, &[]),
            [
                "the session was stopped: reply reached the output limit".to_owned(),
                ERROR.to_owned()
            ]
        );
    }

    #[test]
    fn a_reply_cut_at_the_output_limit_without_a_reason_adds_nothing() {
        assert_eq!(
            diagnose(errors(), &cut_at_output_limit(None), &[]),
            errors()
        );
    }

    #[test]
    fn a_session_aborted_for_another_reason_adds_nothing() {
        let metrics = SessionMetrics {
            abort_reason: Some("loop detected".to_owned()),
            ..SessionMetrics::default()
        };
        assert_eq!(diagnose(errors(), &metrics, &[]), errors());
    }

    #[test]
    fn reverted_files_are_named_before_the_errors_they_caused() {
        assert_eq!(
            diagnose(errors(), &SessionMetrics::default(), &reverted()),
            [
                reverted_note(Role::Bootstrap, &reverted()),
                ERROR.to_owned()
            ]
        );
    }

    #[test]
    fn session_problems_come_first_in_the_order_tools_then_stop_then_reverts() {
        let metrics = SessionMetrics {
            last_stop_reason: Some(StopReason::MaxTokens),
            abort_reason: Some("cut".to_owned()),
            ..tools_failed()
        };
        assert_eq!(
            diagnose(errors(), &metrics, &reverted()),
            [
                tools_failed_message(),
                "the session was stopped: cut".to_owned(),
                reverted_note(Role::Bootstrap, &reverted()),
                ERROR.to_owned()
            ]
        );
    }

    #[test]
    fn files_written_outside_the_project_are_named_after_reverts_and_before_errors() {
        let metrics = SessionMetrics {
            outside_writes: vec![
                "/work/.tenaz/spec.md".to_owned(),
                "/home/me/.tenaz/map.md".to_owned(),
            ],
            ..SessionMetrics::default()
        };
        let problems = diagnose(errors(), &metrics, &reverted());
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert_eq!(problems[0], reverted_note(Role::Bootstrap, &reverted()));
        assert!(
            problems[1].contains("outside the project")
                && problems[1].contains("/work/.tenaz/spec.md, /home/me/.tenaz/map.md"),
            "{problems:?}"
        );
        assert_eq!(problems[2], ERROR);
    }

    #[test]
    fn fewer_problems_than_before_is_progress() {
        assert_eq!(progress(Some(3), 2, 0, 3), Some(3));
    }

    #[test]
    fn as_many_problems_as_before_is_not_progress() {
        assert_eq!(progress(Some(3), 3, 0, 3), None);
    }

    #[test]
    fn more_problems_than_before_is_not_progress() {
        assert_eq!(progress(Some(3), 4, 0, 3), None);
    }

    #[test]
    fn the_first_attempt_is_never_progress() {
        assert_eq!(progress(None, 1, 0, 3), None);
    }

    #[test]
    fn the_last_free_attempt_is_still_progress() {
        assert_eq!(progress(Some(3), 2, 2, 3), Some(3));
    }

    #[test]
    fn an_attempt_that_made_progress_counts_after_max_uncounted_ones() {
        assert_eq!(progress(Some(3), 2, 3, 3), None);
    }

    #[test]
    fn the_log_names_the_first_failed_tool_call() {
        assert_eq!(
            tool_failure_note(&tools_failed()),
            format!("; all 3 tool calls failed, the first: {FIRST_TOOL_ERROR}")
        );
    }

    #[test]
    fn the_log_adds_nothing_when_some_tool_calls_worked() {
        assert_eq!(tool_failure_note(&tool_calls(2)), "");
    }

    #[test]
    fn the_block_lists_the_last_problems() {
        let last = ["a".to_owned(), "b".to_owned()];
        assert_eq!(
            exhausted(
                BootstrapStep::Spec,
                attempts(3, 0),
                HarnessKind::Pi,
                Some(&last)
            ),
            format!(
                "bootstrap failed 3 times at step {}: a; b",
                BootstrapStep::Spec
            )
        );
    }

    #[test]
    fn the_block_blames_the_harness_when_every_attempt_failed_its_tools() {
        assert_eq!(
            exhausted(
                BootstrapStep::Tasks,
                attempts(3, 3),
                HarnessKind::Pi,
                Some(&errors())
            ),
            format!(
                "bootstrap failed 3 times at step {} (in every attempt all of the model's tool \
                 calls failed: the model does not work with pi's tools; try another model or \
                 the other harness): {ERROR}",
                BootstrapStep::Tasks
            )
        );
    }

    #[test]
    fn the_block_does_not_blame_the_harness_when_one_attempt_used_its_tools() {
        assert_eq!(
            exhausted(
                BootstrapStep::Spec,
                attempts(3, 2),
                HarnessKind::Pi,
                Some(&errors())
            ),
            format!(
                "bootstrap failed 3 times at step {}: {ERROR}",
                BootstrapStep::Spec
            )
        );
    }
}
