//! A harness that dies mid-turn between tool calls is an engine-side
//! failure: the session is journaled with its exit and stderr, rerun without
//! charging the task an attempt, and the run still completes. One that dies
//! while a tool call runs (fx on output that is not UTF-8, in the Wild West
//! run of 2026-10-24) would die again on a rerun that makes the same call, so
//! the attempt counts and the next briefing names the call. So does one
//! whose reply was cut at the output limit while the model reasoned (fx
//! exits on `OutputTruncated`): the next session runs at a lower effort.
//! The bootstrap runs in steps, and a step that fails is retried with the
//! files the earlier session left.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{deps, fake_project};
use crate::effort::EffortDial;
use crate::ledger::{BootstrapStep, JournalRecord, Phase};
use crate::orchestrator::Deps;
use crate::project::Project;
use crate::testing::{FastClock, ScriptedHarness};

/// Runs the fake goal to the end with `harness` and `maxAttempts`; the
/// project and its log.
async fn run(harness: ScriptedHarness, max_attempts: u32) -> (tempfile::TempDir, Project, String) {
    run_with(harness, max_attempts, &[]).await
}

/// [`run`] with more parameter overrides.
async fn run_with(
    harness: ScriptedHarness,
    max_attempts: u32,
    set: &[&str],
) -> (tempfile::TempDir, Project, String) {
    let work = tempfile::tempdir().unwrap();
    let attempts = format!("maxAttempts={max_attempts}");
    let mut overrides = vec![attempts.as_str(), "replanEvery=2"];
    overrides.extend_from_slice(set);
    let (project, vcs) = fake_project(work.path(), &overrides);
    let clock = Arc::new(FastClock::new(100));
    let deps = Deps {
        effort: Some(EffortDial::new()),
        ..deps(&project, vcs, Arc::new(harness), clock)
    };
    super::run(&project, deps, CancellationToken::new()).await;
    let log = project.read(".tenaz/private/orchestrator.log").unwrap();
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
    (work, project, log)
}

#[tokio::test]
async fn a_harness_exit_mid_turn_is_rerun_without_charging_the_task() {
    let (_work, project, log) = run(
        ScriptedHarness {
            crash_task: Some("T001".into()),
            ..Default::default()
        },
        2,
    )
    .await;
    let journal = project.journal();
    let crashed = journal
        .iter()
        .find_map(|e| match &e.record {
            JournalRecord::Session(s) if s.harness_exit.is_some() => Some(s.clone()),
            _ => None,
        })
        .expect("the crashed session is journaled with its harness exit");
    assert!(
        crashed.label.ends_with("-builder-T001"),
        "{}",
        crashed.label
    );
    assert_eq!(crashed.attempt, Some(1));
    assert!(!crashed.ok);
    let note = crashed.harness_exit.as_deref().unwrap();
    assert!(note.contains("exit code 1"), "{note}");
    assert!(note.contains("panic: reached unreachable code"), "{note}");
    assert!(
        journal.iter().any(|e| matches!(&e.record, JournalRecord::EngineRetry { label, cause: Some(c), .. } if *label == crashed.label && c == "harness")),
        "the crash was not retried as a harness failure"
    );
    // The rerun is the same attempt: the crash cost T001 nothing.
    let rerun = journal.iter().filter_map(|e| match &e.record {
        JournalRecord::Session(s)
            if s.label.ends_with("-builder-T001") && s.label != crashed.label =>
        {
            Some(s.attempt)
        }
        _ => None,
    });
    assert_eq!(
        rerun.clone().next(),
        Some(Some(1)),
        "the first rerun is still attempt 1"
    );
    assert!(
        log.contains("the harness ended before the turn settled; retrying the same session"),
        "{log}"
    );
}

#[tokio::test]
async fn a_harness_exit_during_a_tool_call_charges_the_attempt_and_names_the_call() {
    // The crash takes one of T001's attempts; the fake run needs two more.
    let (_work, project, log) = run(
        ScriptedHarness {
            crash_in_tool_task: Some("T001".into()),
            ..Default::default()
        },
        3,
    )
    .await;
    let journal = project.journal();
    let crashed = journal
        .iter()
        .find_map(|e| match &e.record {
            JournalRecord::Session(s) if s.harness_exit.is_some() => Some(s.clone()),
            _ => None,
        })
        .expect("the crashed session is journaled with its harness exit");
    assert_eq!(crashed.attempt, Some(1));
    let note = crashed.harness_exit.as_deref().unwrap();
    assert!(note.contains("did not contain valid UTF-8"), "{note}");
    let reason = crashed.abort_reason.as_deref().unwrap();
    assert!(
        reason.contains("head -c 400 checks/result.json"),
        "{reason}"
    );
    assert!(reason.contains("do not cut output by bytes"), "{reason}");
    assert!(
        !journal
            .iter()
            .any(|e| matches!(&e.record, JournalRecord::EngineRetry { label, .. } if *label == crashed.label)),
        "a crash during a tool call is not retried as an engine failure:\n{log}"
    );
    let next = journal
        .iter()
        .find_map(|e| match &e.record {
            JournalRecord::Session(s)
                if s.label.ends_with("-builder-T001") && s.label != crashed.label =>
            {
                Some(s.clone())
            }
            _ => None,
        })
        .expect("T001 runs again");
    assert_eq!(next.attempt, Some(2), "the crash cost T001 an attempt");
    let prompt = project
        .read(&format!(".tenaz/private/{}.prompt.md", next.label))
        .unwrap();
    assert!(
        prompt.contains("head -c 400 checks/result.json"),
        "{prompt}"
    );
    assert!(prompt.contains("do not cut output by bytes"), "{prompt}");
}

#[tokio::test]
async fn a_reply_cut_at_the_output_limit_charges_the_attempt_and_lowers_the_effort() {
    let (_work, project, log) = run(
        ScriptedHarness {
            truncate_task: Some("T001".into()),
            ..Default::default()
        },
        3,
    )
    .await;
    let journal = project.journal();
    let cut = journal
        .iter()
        .find_map(|e| match &e.record {
            JournalRecord::Session(s) if s.harness_exit.is_some() => Some(s.clone()),
            _ => None,
        })
        .expect("the cut session is journaled with its harness exit");
    assert!(cut.label.ends_with("-builder-T001"), "{}", cut.label);
    let reason = cut.abort_reason.as_deref().unwrap();
    assert!(reason.contains("output limit"), "{reason}");
    assert!(
        !journal.iter().any(
            |e| matches!(&e.record, JournalRecord::EngineRetry { label, .. } if *label == cut.label)
        ),
        "a reply cut at the output limit is not retried as an engine failure:\n{log}"
    );
    let next = journal
        .iter()
        .find_map(|e| match &e.record {
            JournalRecord::Session(s)
                if s.label.ends_with("-builder-T001") && s.label != cut.label =>
            {
                Some(s.clone())
            }
            _ => None,
        })
        .expect("T001 runs again");
    assert_eq!(next.attempt, Some(2), "the cut cost T001 an attempt");
    let prompt = project
        .read(&format!(".tenaz/private/{}.prompt.md", next.label))
        .unwrap();
    assert!(prompt.contains("reason briefly"), "{prompt}");
    assert!(
        log.contains(&format!(
            "{}: the last reply stopped at the output limit; this session reasons at a lower effort",
            next.label
        )),
        "{log}"
    );
    assert!(
        !project.load_ledger().unwrap().lower_effort,
        "one session only"
    );
}

#[tokio::test]
async fn a_bootstrap_cut_at_the_output_limit_is_told_why_and_tried_again() {
    let (_work, project, log) = run(
        ScriptedHarness {
            truncate_bootstrap: true,
            ..Default::default()
        },
        2,
    )
    .await;
    let journal = project.journal();
    let problems = journal
        .iter()
        .find_map(|e| match &e.record {
            JournalRecord::BootstrapValidation { errors, .. } if !errors.is_empty() => {
                Some(errors.clone())
            }
            _ => None,
        })
        .expect("the cut bootstrap left problems");
    assert!(
        problems[0].starts_with("the session was stopped: the reply reached the output limit"),
        "{problems:?}"
    );
    assert!(
        log.contains("this session reasons at a lower effort"),
        "{log}"
    );
}

#[tokio::test]
async fn the_next_bootstrap_is_told_which_files_the_previous_one_left() {
    let (_work, project, _log) = run(
        ScriptedHarness {
            partial_bootstrap: true,
            ..Default::default()
        },
        2,
    )
    .await;
    let labels: Vec<String> = project
        .journal()
        .iter()
        .filter_map(|e| match &e.record {
            JournalRecord::Session(s) if s.label.ends_with("-bootstrap") => Some(s.label.clone()),
            _ => None,
        })
        .collect();
    // The fake's first bootstrap sessions hit engine errors; the partial
    // one and the full one are the last two.
    let (last, earlier) = labels.split_last().unwrap();
    let prompt = |label: &str| {
        project
            .read(&format!(".tenaz/private/{label}.prompt.md"))
            .unwrap()
    };
    for label in earlier {
        assert!(!prompt(label).contains("previous bootstrap sessions left"));
    }
    let second = prompt(last);
    let left = second
        .split("Files the previous bootstrap sessions left (kept)")
        .nth(1)
        .expect("the second bootstrap gets the files the first one left");
    assert!(left.contains(".tenaz/spec.md"), "{second}");
    assert!(left.contains("index.html"), "{second}");
    assert!(left.contains("Do not start over"), "{second}");
}

#[tokio::test]
async fn the_bootstrap_runs_one_session_per_step() {
    let (_work, project, log) = run(
        ScriptedHarness {
            stepwise_bootstrap: true,
            ..Default::default()
        },
        2,
    )
    .await;
    let journal = project.journal();
    let passed: Vec<BootstrapStep> = journal
        .iter()
        .filter_map(|e| match &e.record {
            JournalRecord::BootstrapValidation { step, errors, .. } if errors.is_empty() => {
                Some(*step)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        passed,
        vec![
            BootstrapStep::Spec,
            BootstrapStep::Verify,
            BootstrapStep::Tasks
        ],
        "{log}"
    );
    let labels: Vec<String> = journal
        .iter()
        .filter_map(|e| match &e.record {
            JournalRecord::Session(s) if s.label.ends_with("-bootstrap") => Some(s.label.clone()),
            _ => None,
        })
        .collect();
    // The fake's first bootstrap sessions hit engine errors.
    let [.., first, second, last] = labels.as_slice() else {
        panic!("three step sessions: {labels:?}");
    };
    let prompt = |label: &str| {
        project
            .read(&format!(".tenaz/private/{label}.prompt.md"))
            .unwrap()
    };
    let first = prompt(first);
    assert!(first.contains("Step: 1 of 3 (spec and map)"), "{first}");
    assert!(first.contains("## Step 1: spec and map"), "{first}");
    assert!(!first.contains("## Step 2"), "{first}");
    let second = prompt(second);
    assert!(second.contains("Step: 2 of 3 (verify)"), "{second}");
    assert!(second.contains("write no other checks now"), "{second}");
    assert!(!second.contains("## Step 3"), "{second}");
    let last = prompt(last);
    assert!(last.contains("## Step 3: task cards"), "{last}");
    assert!(!last.contains("## Step 1"), "{last}");
    assert!(
        last.contains("## Spec (.tenaz/spec.md), from step 1"),
        "{last}"
    );
    assert!(last.contains("- R1: one"), "{last}");
    assert!(
        log.contains("Bootstrap step 1 of 3 (spec and map) done"),
        "{log}"
    );
}

#[tokio::test]
async fn bootstrap_steps_already_done_need_no_session() {
    let (_work, project, log) = run(ScriptedHarness::default(), 2).await;
    let steps: Vec<BootstrapStep> = project
        .journal()
        .iter()
        .filter_map(|e| match &e.record {
            JournalRecord::BootstrapValidation { step, .. } => Some(*step),
            _ => None,
        })
        .collect();
    assert_eq!(steps, vec![BootstrapStep::Spec], "{log}");
    assert!(
        log.contains("Bootstrap step 2 of 3 (verify) is already done"),
        "{log}"
    );
    assert!(
        log.contains("Bootstrap step 3 of 3 (task cards) is already done"),
        "{log}"
    );
}

#[tokio::test]
async fn a_reviewer_over_its_context_is_asked_for_its_verdict_not_notes() {
    let (_work, project, log) = run(
        ScriptedHarness {
            outgrow_review: Some("T002".into()),
            ..Default::default()
        },
        3,
    )
    .await;
    let steers = project
        .read(".tenaz/private/fake-steers.log")
        .unwrap_or_default();
    let reviewer: Vec<&str> = steers
        .lines()
        .filter(|l| l.starts_with("REVIEWER: ") && l.contains("tokens of context"))
        .collect();
    assert!(!reviewer.is_empty(), "the reviewer was steered:\n{log}");
    for line in reviewer {
        assert!(line.contains("write your verdict file now"), "{line}");
        assert!(!line.contains("handoff notes"), "{line}");
    }
}

#[tokio::test]
async fn a_review_with_minor_findings_only_accepts_the_task() {
    let (_work, project, log) = run(
        ScriptedHarness {
            minor_review: Some("T002".into()),
            ..Default::default()
        },
        3,
    )
    .await;
    assert!(
        log.contains("T002: the review asks for changes with MINOR findings only; accepted"),
        "{log}"
    );
    // The fake's first T002 attempt fails on its own; the reviewed one is the second.
    let reviews = project
        .journal()
        .iter()
        .filter(
            |e| matches!(&e.record, JournalRecord::Review { task, .. } if task.as_str() == "T002"),
        )
        .count();
    assert_eq!(reviews, 1, "no further attempt for a MINOR finding:\n{log}");
}

#[tokio::test]
async fn the_reviewer_is_given_the_images_the_change_adds() {
    let (_work, project, log) = run(
        ScriptedHarness {
            image_task: Some("T004".into()),
            ..Default::default()
        },
        3,
    )
    .await;
    let reviewers: Vec<String> = project
        .journal()
        .iter()
        .filter_map(|e| match &e.record {
            JournalRecord::Session(s) if s.label.contains("-reviewer-") => Some(s.label.clone()),
            _ => None,
        })
        .collect();
    let prompt = |label: &str| {
        project
            .read(&format!(".tenaz/private/{label}.prompt.md"))
            .unwrap()
    };
    let t004 = reviewers
        .iter()
        .find(|l| l.ends_with("-reviewer-T004"))
        .expect("T004 is reviewed");
    let text = prompt(t004);
    assert!(
        text.contains("## Images this change adds or replaces\n\n- assets/T004.png"),
        "{text}"
    );
    let t001 = reviewers
        .iter()
        .find(|l| l.ends_with("-reviewer-T001"))
        .expect("T001 is reviewed");
    assert!(
        !prompt(t001).contains("Images this change adds"),
        "a change without images gets no image section:\n{log}"
    );
}

#[tokio::test]
async fn each_request_line_records_what_the_request_asked_for() {
    let (_work, project, _log) = run(ScriptedHarness::default(), 3).await;
    let lines = project.read(".tenaz/private/requests.jsonl").unwrap();
    let recorded: Vec<serde_json::Value> = lines
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .filter(|r: &serde_json::Value| r.get("sent").is_some())
        .collect();
    assert!(!recorded.is_empty(), "{lines}");
    assert_eq!(
        recorded[0]["sent"],
        serde_json::json!({"reasoning_effort": "medium", "max_tokens": 16384})
    );
}

#[tokio::test]
async fn an_attempt_that_leaves_fewer_passing_checks_fails() {
    let (_work, project, log) = run(
        ScriptedHarness {
            drop_check_task: Some("T004".into()),
            ..Default::default()
        },
        3,
    )
    .await;
    let builders: Vec<String> = project
        .journal()
        .iter()
        .filter_map(|e| match &e.record {
            JournalRecord::Session(s) if s.label.ends_with("-builder-T004") => {
                Some(s.label.clone())
            }
            _ => None,
        })
        .collect();
    assert!(
        builders.len() >= 2,
        "T004 is tried again: {builders:?}\n{log}"
    );
    let second = project
        .read(&format!(".tenaz/private/{}.prompt.md", builders[1]))
        .unwrap();
    assert!(second.contains("fewer than the"), "{second}");
    assert!(
        project.exists("checks/T001.exists.sh"),
        "the builder put the check back"
    );
    assert!(log.contains("T004 done (2 attempts"), "{log}");
}

#[tokio::test]
async fn a_builder_stopped_at_its_turn_budget_is_continued_not_charged() {
    let (_work, project, log) = run(
        ScriptedHarness {
            long_task: Some("T001".into()),
            ..Default::default()
        },
        3,
    )
    .await;
    assert!(
        log.contains("stopped at its turn budget; a fresh session continues the same work (1/3, not counted as an attempt)"),
        "{log}"
    );
    let continued = project
        .journal()
        .iter()
        .find_map(|e| match &e.record {
            JournalRecord::TurnContinuation { label, .. } => Some(label.clone()),
            _ => None,
        })
        .expect("the turn stop is journaled");
    assert!(continued.ends_with("-builder-T001"), "{continued}");
    let next = project
        .journal()
        .iter()
        .find_map(|e| match &e.record {
            JournalRecord::Session(s)
                if s.label.ends_with("-builder-T001") && s.label != continued =>
            {
                Some(s.clone())
            }
            _ => None,
        })
        .expect("a fresh session continues T001");
    assert_eq!(next.attempt, Some(1), "the turn stop cost no attempt");
    let prompt = project
        .read(&format!(".tenaz/private/{}.prompt.md", next.label))
        .unwrap();
    assert!(prompt.contains("used up its turn budget"), "{prompt}");
}

#[tokio::test]
async fn the_final_reviewer_judges_the_product_against_the_brief_as_a_whole() {
    let (_work, project, _log) = run(ScriptedHarness::default(), 3).await;
    let label = project
        .journal()
        .iter()
        .find_map(|e| match &e.record {
            JournalRecord::Session(s) if s.label.ends_with("-final-reviewer") => {
                Some(s.label.clone())
            }
            _ => None,
        })
        .expect("a final review ran");
    let prompt = project
        .read(&format!(".tenaz/private/{label}.prompt.md"))
        .unwrap();
    assert!(prompt.contains("judge the product as a whole"), "{prompt}");
    assert!(prompt.contains("under `## Overall`"), "{prompt}");
}

/// A builder's shell call that never ends, as `./verify --all` with an
/// endless loop did under Pi in the Digger 3D run of 2026-10-27, is stopped
/// at `toolMinutes`. The session is told why and goes on with its context
/// to do the task.
#[tokio::test]
async fn a_tool_call_over_its_time_is_stopped_and_the_session_goes_on() {
    let (_work, project, log) = run_with(
        ScriptedHarness {
            hang_task: Some("T001".into()),
            hang_times: 1,
            ..Default::default()
        },
        3,
        &["toolMinutes=1"],
    )
    .await;
    assert!(
        log.contains("tool call over its time limit; stopped, the session goes on"),
        "{log}"
    );
    let sessions: Vec<_> = project
        .journal()
        .iter()
        .filter_map(|e| match &e.record {
            JournalRecord::Session(s) if s.label.ends_with("-builder-T001") => Some(s.clone()),
            _ => None,
        })
        .collect();
    // The fake's first T001 session that does the work then outgrows its
    // context on purpose; that, not the tool call, is what ends it.
    let first = &sessions[0];
    assert_eq!(first.attempt, Some(1));
    assert!(
        first
            .abort_reason
            .as_deref()
            .is_some_and(|r| r.starts_with("context budget exceeded")),
        "{first:?}"
    );
    let stopped = first
        .watchdog
        .iter()
        .find(|e| e.kind == "tool-timeout")
        .expect("the timeout is in the session record");
    assert_eq!(stopped.detail.as_deref(), Some("./verify --all"));
    let raw = project
        .read(&format!(".tenaz/private/{}.rpc.jsonl", first.label))
        .unwrap();
    assert!(
        raw.contains("src/T001.txt"),
        "the session went on to do the task"
    );
}

/// A second call over the time limit in the same session ends it, and the
/// attempt counts.
#[tokio::test]
async fn a_second_tool_call_over_its_time_ends_the_session() {
    let (_work, project, log) = run_with(
        ScriptedHarness {
            hang_task: Some("T001".into()),
            hang_times: 2,
            ..Default::default()
        },
        3,
        &["toolMinutes=1"],
    )
    .await;
    assert!(
        log.contains("abort — a tool call ran over 1 minutes twice"),
        "{log}"
    );
    let sessions: Vec<_> = project
        .journal()
        .iter()
        .filter_map(|e| match &e.record {
            JournalRecord::Session(s) if s.label.ends_with("-builder-T001") => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(!sessions[0].ok);
    assert_eq!(
        sessions[0].abort_reason.as_deref(),
        Some("a tool call ran over 1 minutes twice")
    );
    let second = sessions
        .iter()
        .find(|s| s.attempt == Some(2) && s.ok)
        .unwrap_or_else(|| panic!("the next attempt does the task: {sessions:?}"));
    // It is told why the first attempt's session was stopped.
    let prompt = project
        .read(&format!(".tenaz/private/{}.prompt.md", second.label))
        .unwrap();
    assert!(
        prompt.contains("The session was stopped: a tool call ran over 1 minutes twice.\n"),
        "{prompt}"
    );
}

/// A reply that crosses the context stop with the task's whole file in its
/// write, as in the Digger 3D run of 2026-10-27, where the stop cancelled
/// the write three times: the write lands, then the session stops.
#[tokio::test]
async fn the_write_in_the_reply_that_crosses_the_context_stop_lands() {
    let (_work, project, log) = run(
        ScriptedHarness {
            big_write_task: Some("T001".into()),
            ..Default::default()
        },
        3,
    )
    .await;
    let first = project
        .journal()
        .iter()
        .find_map(|e| match &e.record {
            JournalRecord::Session(s) if s.label.ends_with("-builder-T001") => Some(s.clone()),
            _ => None,
        })
        .expect("T001 has a builder session");
    assert!(
        first
            .abort_reason
            .as_deref()
            .is_some_and(|r| r.starts_with("context budget exceeded")),
        "{first:?}"
    );
    let raw = project
        .read(&format!(".tenaz/private/{}.rpc.jsonl", first.label))
        .unwrap();
    let write = raw
        .lines()
        .filter_map(|l| serde_json::from_str::<crate::session::SessionEvent>(l).ok())
        .find_map(|e| match e {
            crate::session::SessionEvent::ToolCallEnd {
                name,
                result_text,
                is_error,
                ..
            } if name == "write" => Some((result_text, is_error)),
            _ => None,
        });
    assert_eq!(
        write,
        Some(("ok".to_owned(), false)),
        "the write ran before the stop"
    );
    assert!(
        log.contains("stopped at its context budget; a fresh session continues"),
        "{log}"
    );
}
