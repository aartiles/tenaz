//! The engine preflight in whole runs: an engine that cannot take its
//! requests stops the run with its own error, and a weak engine stops it
//! unless `--allow-weak-engine` accepts it.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{deps, fake_project, try_run};
use crate::cache_report::CacheReport;
use crate::orchestrator::{Deps, RunError, RunOptions};
use crate::project::Project;
use crate::testing::{ScriptedHarness, SnapshotVcs, VirtualClock};

/// Runs `project` once with `harness`, behind an engine proxy that saw the
/// engine name its cached tokens, with `--allow-weak-engine` when
/// `allow_weak_engine`.
async fn run_once(
    project: &Project,
    vcs: Arc<SnapshotVcs>,
    harness: ScriptedHarness,
    allow_weak_engine: bool,
) -> Result<(), RunError> {
    // An engine that names its cached tokens: a second answer that reads
    // none fails the cache check instead of passing with a warning.
    let report = CacheReport::new();
    report.note(true);
    let deps = Deps {
        cache_report: Some(report),
        ..deps(
            project,
            vcs,
            Arc::new(harness),
            Arc::new(VirtualClock::new()),
        )
    };
    let options = RunOptions {
        allow_weak_engine,
        ..RunOptions::default()
    };
    try_run(project, deps, CancellationToken::new(), options).await
}

/// An engine that refuses every preflight request stops the run with its
/// own error, even with `--allow-weak-engine`, without blaming the model;
/// the next run measures the engine again.
#[tokio::test(start_paused = true)]
async fn an_engine_that_refuses_the_preflight_stops_the_run_with_its_error() {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &[]);
    let refusing = ScriptedHarness {
        refused_preflight: Some("503: not enough memory".into()),
        ..ScriptedHarness::default()
    };
    let error = run_once(&project, vcs.clone(), refusing, true)
        .await
        .unwrap_err();
    assert!(matches!(error, RunError::Precondition(_)), "{error:?}");
    let text = error.to_string();
    assert!(
        text.contains("its requests: 503: not enough memory."),
        "{text}"
    );
    for blame in [
        "no reasoning",
        "cached prompt prefix",
        "shell tool",
        "allow-weak-engine",
    ] {
        assert!(!text.contains(blame), "{blame} in {text}");
    }
    assert!(
        text.ends_with("Details in .tenaz/run.json \"preflight\"."),
        "{text}"
    );
    let preflight = project.load_ledger().unwrap().preflight.unwrap();
    assert!(!preflight.ok && !preflight.accepted_weak);
    let saved = run_json_preflight(&project);
    assert_eq!(saved["engine_errors"].as_array().map(Vec::len), Some(3));
    let log = project.read(".tenaz/private/orchestrator.log").unwrap();
    assert!(
        log.contains("preflight: the engine answered 503: not enough memory"),
        "{log}"
    );
    assert_eq!(
        preflight_line_in_status_and_log(&project),
        "Preflight: not judged: the engine could not take some of its requests engine_errors=3"
    );

    run_once(&project, vcs, ScriptedHarness::default(), false)
        .await
        .unwrap();
    let preflight = project.load_ledger().unwrap().preflight.unwrap();
    assert!(preflight.ok);
    assert!(run_json_preflight(&project).get("engine_errors").is_none());
}

/// The `preflight` object of `project`'s `run.json`, as scripts read it.
fn run_json_preflight(project: &Project) -> serde_json::Value {
    let run: serde_json::Value =
        serde_json::from_str(&project.read(".tenaz/run.json").unwrap()).unwrap();
    run["preflight"].clone()
}

/// An engine that reads nothing from its prefix cache stops the run, and
/// `--allow-weak-engine` records the failure and runs anyway; the log's
/// preflight line then says it was accepted, as `tenaz status` does.
#[tokio::test(start_paused = true)]
async fn a_weak_engine_stops_the_run_unless_accepted() {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &["maxAttempts=2", "replanEvery=2"]);
    let weak = || ScriptedHarness {
        uncached: true,
        ..ScriptedHarness::default()
    };
    let text = run_once(&project, vcs.clone(), weak(), false)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        text.contains("did not reuse the cached prompt prefix"),
        "{text}"
    );
    assert!(
        text.ends_with(
            "Details in .tenaz/run.json \"preflight\"; pass --allow-weak-engine to run anyway."
        ),
        "{text}"
    );
    let line = preflight_line_in_status_and_log(&project);
    assert!(!line.ends_with(" (accepted weak)"), "{line}");

    run_once(&project, vcs, weak(), true).await.unwrap();
    let ledger = project.load_ledger().unwrap();
    assert!(ledger.preflight.is_some_and(|p| p.ok && p.accepted_weak));
    assert_eq!(ledger.phase, crate::ledger::Phase::Complete);
    let line = preflight_line_in_status_and_log(&project);
    assert!(line.ends_with(" (accepted weak)"), "{line}");
}

/// The preflight line of `tenaz status` for `project`, which must also be
/// the message of a line of the run's log (`[time] message`).
fn preflight_line_in_status_and_log(project: &Project) -> String {
    let status = crate::orchestrator::status_text(project);
    let line = status
        .lines()
        .find(|l| l.starts_with("Preflight: "))
        .unwrap_or_else(|| panic!("{status}"));
    let log = project.read(".tenaz/private/orchestrator.log").unwrap();
    assert!(
        log.lines().any(|l| l
            .split_once("] ")
            .is_some_and(|(_, message)| message == line)),
        "{line:?} not in {log}"
    );
    line.to_owned()
}
