//! Runs where some roles play another model than the run's: each session
//! is opened with its role's model and budgeted by that model's window
//! (ADR 0010).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::{deps, fake_project, run};
use crate::briefing;
use crate::ledger::{JournalRecord, SessionRecord};
use crate::params::RunParams;
use crate::ports::{Harness, HarnessError, HarnessInfo, Session, SessionModel, SessionSpec};
use crate::project::Project;
use crate::role::Role;
use crate::role_models::RoleModel;
use crate::testing::{ScriptedHarness, VirtualClock};

/// The scripted harness, keeping the model each session was opened with.
struct Recording {
    scripted: ScriptedHarness,
    opened: Mutex<Vec<(String, SessionModel)>>,
}

#[async_trait]
impl Harness for Recording {
    fn info(&self) -> HarnessInfo {
        self.scripted.info()
    }

    async fn open(&self, spec: SessionSpec) -> Result<Box<dyn Session>, HarnessError> {
        self.opened
            .lock()
            .unwrap()
            .push((spec.label.clone(), spec.model.clone()));
        self.scripted.open(spec).await
    }
}

/// A run played to its end with some roles pinned to their own models.
struct Played {
    _work: tempfile::TempDir,
    project: Project,
    records: Vec<JournalRecord>,
    opened: Vec<(String, SessionModel)>,
}

impl Played {
    async fn with(scripted: ScriptedHarness, roles: &[(Role, RoleModel)]) -> Self {
        let work = tempfile::tempdir().unwrap();
        // A replan every two tasks, so every role plays.
        let (project, vcs) = fake_project(work.path(), &["maxAttempts=2", "replanEvery=2"]);
        let mut ledger = project.load_ledger().unwrap();
        ledger.config.role_models.extend(roles.iter().cloned());
        project.save_ledger(&ledger).unwrap();
        let harness = Arc::new(Recording {
            scripted,
            opened: Mutex::default(),
        });
        let deps = deps(
            &project,
            vcs,
            harness.clone(),
            Arc::new(VirtualClock::new()),
        );
        run(&project, deps, CancellationToken::new()).await;
        let records = project.journal().into_iter().map(|e| e.record).collect();
        let opened = harness.opened.lock().unwrap().clone();
        Self {
            _work: work,
            project,
            records,
            opened,
        }
    }

    fn sessions(&self) -> impl Iterator<Item = &SessionRecord> {
        self.records.iter().filter_map(session)
    }

    /// The index of the first record that passes `test`.
    fn first(&self, test: impl Fn(&JournalRecord) -> bool) -> usize {
        let found = self.records.iter().position(test);
        found.unwrap_or_else(|| panic!("no such record: {:#?}", self.records))
    }

    fn run_model(&self) -> SessionModel {
        self.project.load_ledger().unwrap().config.run_model()
    }
}

fn session(record: &JournalRecord) -> Option<&SessionRecord> {
    match record {
        JournalRecord::Session(s) => Some(s),
        _ => None,
    }
}

fn is_raise(record: &JournalRecord) -> bool {
    matches!(record, JournalRecord::ContextCapRaised { .. })
}

fn role_model(id: &str, context_window: u64) -> RoleModel {
    RoleModel {
        model: format!("fake/{id}"),
        id: id.into(),
        context_window,
        input: vec!["text".into()],
    }
}

#[tokio::test(start_paused = true)]
async fn each_session_runs_its_role_s_model_within_that_model_s_budget() {
    let critic = role_model("critic", 65536);
    // A task that cannot pass is split, so the split plays too.
    let scripted = ScriptedHarness {
        failing_task: Some("T003".into()),
        ..Default::default()
    };
    let run = Played::with(scripted, &[(Role::Reviewer, critic.clone())]).await;
    let run_model = run.run_model();
    let roles: Vec<Role> = run.sessions().map(|s| s.role).collect();
    for role in Role::ALL {
        assert!(roles.contains(&role), "no {role} session: {roles:?}");
    }
    // The preflight probes the run's model, and every other spec opened
    // belongs to a journaled session.
    assert_eq!(run.opened[0], ("preflight".to_owned(), run_model.clone()));
    assert_eq!(run.opened.len(), roles.len() + 1);
    for session in run.sessions() {
        // The default share of each window: 0.65 of 65536 and of 131072.
        let (expected, budget) = match session.role {
            Role::Reviewer => (critic.session_model(), 42598),
            _ => (run_model.clone(), 85196),
        };
        let label = &session.label;
        assert_eq!(session.model.as_ref(), Some(&expected.name), "{label}");
        assert_eq!(session.context_budget, budget, "{label}");
        let opened = run.opened.iter().find(|(l, _)| l == label);
        assert_eq!(opened, Some(&(label.clone(), expected)));
    }
}

/// Cards are sized to the builder's budget, from the builder's model.
#[tokio::test(start_paused = true)]
async fn the_planners_size_cards_for_the_builder_s_model() {
    let builder = role_model("builder", 65536);
    let run = Played::with(ScriptedHarness::default(), &[(Role::Builder, builder)]).await;
    let prompts: Vec<String> = std::fs::read_dir(run.project.private_dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".prompt.md"))
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .collect();
    let sized = |budget| {
        let text = briefing::card_sizing(budget);
        prompts.iter().any(|p| p.contains(&text))
    };
    assert!(sized(42598), "no prompt sizes cards for 42598 tokens");
    assert!(!sized(85196), "a prompt sizes cards for the run's model");
}

/// After the engine runs out of memory, the lowered budget is raised
/// toward the largest model's budget, not only the run's.
#[tokio::test(start_paused = true)]
async fn a_raise_aims_at_the_largest_model_s_budget() {
    let scripted = ScriptedHarness {
        engine_failures: true,
        ..Default::default()
    };
    let critic = role_model("critic", 262_144);
    let run = Played::with(scripted, &[(Role::Reviewer, critic)]).await;
    // Halfway from 49190 to 0.65 of 262144 (170393), not to 85196.
    let raised = run.first(is_raise);
    assert!(
        matches!(
            run.records[raised],
            JournalRecord::ContextCapRaised {
                context_cap: Some(109_791),
                ..
            }
        ),
        "{:#?}",
        run.records[raised]
    );
    // Above the run's budget now, the cap holds back only the critic.
    let after: Vec<&SessionRecord> = run.records[raised..].iter().filter_map(session).collect();
    let budgets = |role| -> Vec<u64> {
        after
            .iter()
            .filter(|s| s.role == role)
            .map(|s| s.context_budget)
            .collect()
    };
    let builders = budgets(Role::Builder);
    assert!(
        !builders.is_empty() && builders.iter().all(|b| *b == 85196),
        "{builders:?}"
    );
    let reviewers = budgets(Role::Reviewer);
    assert!(reviewers.contains(&109_791), "{reviewers:?}");
}

/// A session whose budget is under the lowered cap does not count toward
/// raising it: it did not test what the engine can hold.
#[tokio::test(start_paused = true)]
async fn sessions_under_the_cap_do_not_count_toward_a_raise() {
    let scripted = ScriptedHarness {
        engine_failures: true,
        ..Default::default()
    };
    // 0.65 of 32768 is 21299, under the 49190 cap.
    let critic = role_model("critic", 32768);
    let run = Played::with(scripted, &[(Role::Reviewer, critic)]).await;
    let raised = run.first(is_raise);
    let lowered = run.records[..raised]
        .iter()
        .rposition(|r| {
            matches!(
                r,
                JournalRecord::ContextCap {
                    context_cap: 49190,
                    ..
                }
            )
        })
        .expect("the cap was lowered before it was raised");
    let held: Vec<u64> = run.records[lowered..raised]
        .iter()
        .filter_map(session)
        .filter(|s| s.ok)
        .map(|s| s.context_budget)
        .collect();
    // `capacityRecoverSessions` sessions at the cap raise it; the critic's
    // sessions at 21299 ran meanwhile and did not count.
    let needed = RunParams::default().capacity_recover_sessions;
    let at_cap = held.iter().filter(|b| **b == 49190).count();
    assert_eq!(u32::try_from(at_cap).unwrap(), needed, "{held:?}");
    assert!(
        matches!(
            run.records[raised],
            JournalRecord::ContextCapRaised { clean_sessions, .. } if clean_sessions == needed
        ),
        "{:#?}",
        run.records[raised]
    );
    assert!(held.contains(&21299), "{held:?}");
}
