//! A request the engine refused for its output limit, and the engine
//! proxy sent again with the limit the engine had room for: the session
//! goes on and its journal record says so.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{deps, fake_project, run};
use crate::ledger::{JournalRecord, Phase};
use crate::session::OUTPUT_LIMIT_LOWERED;
use crate::testing::{ScriptedHarness, VirtualClock};

#[tokio::test(start_paused = true)]
async fn a_lowered_output_limit_is_journaled_with_the_session() {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &[]);
    let harness = ScriptedHarness {
        lowered_limit_task: Some("T002".into()),
        ..Default::default()
    };
    let deps = deps(
        &project,
        vcs,
        Arc::new(harness),
        Arc::new(VirtualClock::new()),
    );
    run(&project, deps, CancellationToken::new()).await;
    assert_eq!(project.load_ledger().unwrap().phase, Phase::Complete);
    let sessions: Vec<_> = project
        .journal()
        .into_iter()
        .filter_map(|entry| match entry.record {
            JournalRecord::Session(s) if s.label.ends_with("-builder-T002") => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(sessions.len(), 1, "{sessions:#?}");
    assert!(sessions[0].ok);
    assert!(
        sessions[0]
            .watchdog
            .iter()
            .any(|e| e.kind == OUTPUT_LIMIT_LOWERED),
        "{:?}",
        sessions[0].watchdog
    );
}
