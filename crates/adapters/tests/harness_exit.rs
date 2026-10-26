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

use std::path::Path;
use std::sync::Arc;

use tenaz_adapters::git::GitRepo;
use tenaz_adapters::notify::SilentNotifier;
use tenaz_adapters::verify::VerifyProcess;
use tenaz_core::ledger::{BootstrapStep, JournalRecord, ModelConfig, Phase};
use tenaz_core::orchestrator::{Deps, InitOptions, RunOptions, Runner, Templates, init};
use tenaz_core::params::RunParams;
use tenaz_core::project::Project;
use tenaz_core::testing::{FastClock, ScriptedHarness};
use tokio_util::sync::CancellationToken;

/// Runs the fake goal to the end with `harness` and `maxAttempts`; the
/// project and its log.
async fn run(harness: ScriptedHarness, max_attempts: u32) -> (tempfile::TempDir, Project, String) {
    let work = tempfile::tempdir().unwrap();
    let brief = work.path().join("brief.md");
    std::fs::write(&brief, "# Fake brief\n\nBuild four trivial files.\n").unwrap();
    let project = Project::new(work.path().join("project"));
    let mut params = RunParams::default();
    for pair in [
        &format!("maxAttempts={max_attempts}"),
        "replanEvery=2",
        "engineRetryBaseSec=0",
        "notify=false",
    ] {
        params.apply_override(pair).unwrap();
    }
    let config = ModelConfig {
        harness: "fake".into(),
        harness_version: "fake-pi 0.0.1".into(),
        model: "fake/fake".into(),
        provider: Some("fake".into()),
        id: "fake".into(),
        base_url: None,
        context_window: 131072,
        input: vec!["text".into()],
        thinking: Some("medium".into()),
    };
    let vcs = GitRepo::new(project.root());
    let templates =
        Templates::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../cli/templates")).unwrap();
    let options = InitOptions {
        launch: tenaz_core::ledger::Launch::default(),
        brief: Some(brief),
        config,
        params,
        skills: vec![],
        runner_version: "test".into(),
    };
    init(&project, &vcs, &FastClock::new(100), &templates, options).unwrap();

    let deps = Deps {
        harness: Arc::new(harness),
        vcs: Arc::new(GitRepo::new(project.root())),
        verify: Arc::new(VerifyProcess::new(project.root())),
        clock: Arc::new(FastClock::new(100)),
        notifier: Arc::new(SilentNotifier),
        engine: None,
        lease: None,
        engine_metrics: None,
        memory: None,
        effort: Some(tenaz_core::effort::EffortDial::new()),
    };
    let mut runner = Runner::open(project.clone(), deps, CancellationToken::new()).unwrap();
    runner
        .run(RunOptions {
            runner_version: "test".into(),
            ..Default::default()
        })
        .await
        .unwrap();
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
