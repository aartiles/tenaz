//! A builder that declares a heavy job (ADR 0010): the job runs between
//! the sessions of its attempt, the journal records it, and a fresh
//! session of the same attempt is told how it went. A job file that does
//! not stand runs nothing, and the next session is told why. A pause
//! loses neither the job nor its report.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use tokio_util::sync::CancellationToken;

use super::{deps, fake_project, run};
use crate::briefing::OUT_OF_MEMORY;
use crate::lease::{Gate, LeaseManager};
use crate::ledger::JournalRecord::JobFinished;
use crate::ledger::{JournalRecord, Phase};
use crate::ports::{
    Clock, EngineAdapter, EngineError, Harness, HarnessError, HarnessInfo, ModelState, Session,
    SessionSpec,
};
use crate::project::Project;
use crate::testing::{ScriptedHarness, ScriptedJob, ScriptedJobs, SnapshotVcs, VirtualClock};

const RENDER: &str =
    r#"{"command": "sh render.sh", "minutes": 40, "why": "the frames take 25 minutes"}"#;

const REPORT: &str = "## The job you declared\n\n`sh render.sh` exited with";

/// A project of the fake goal, its repository and its work directory.
fn project(overrides: &[&str]) -> (tempfile::TempDir, Project, Arc<SnapshotVcs>) {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), overrides);
    (work, project, vcs)
}

/// T002's first builder declares a job whose file says `text`.
fn first_session_declares(text: &str) -> ScriptedJob {
    ScriptedJob {
        task: "T002".into(),
        text: text.into(),
        every_session: false,
    }
}

/// Runs `project` until it ends or pauses, with `job` declared and `jobs`
/// running them; the run's log.
async fn play(
    project: &Project,
    vcs: &Arc<SnapshotVcs>,
    job: Option<ScriptedJob>,
    jobs: Arc<ScriptedJobs>,
) -> String {
    let harness = ScriptedHarness {
        job,
        ..Default::default()
    };
    let mut deps = deps(
        project,
        vcs.clone(),
        Arc::new(harness),
        Arc::new(VirtualClock::new()),
    );
    deps.jobs = jobs;
    run(project, deps, CancellationToken::new()).await;
    project.read(".tenaz/private/orchestrator.log").unwrap()
}

/// Plays the fake goal with T002's first builder declaring a job whose
/// file says `text`; the project, its log and the commands the jobs ran.
async fn declaring(text: &str) -> (tempfile::TempDir, Project, String, Vec<String>) {
    let (work, project, vcs) = project(&["maxAttempts=3"]);
    let jobs = Arc::new(ScriptedJobs::default());
    let log = play(
        &project,
        &vcs,
        Some(first_session_declares(text)),
        jobs.clone(),
    )
    .await;
    (work, project, log, jobs.ran())
}

/// The prompts of T002's builder sessions, in order.
fn builder_prompts(project: &Project) -> Vec<String> {
    let mut paths: Vec<_> = std::fs::read_dir(project.private_dir())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with("-builder-T002.prompt.md"))
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|p| std::fs::read_to_string(p).unwrap())
        .collect()
}

fn finished(project: &Project) -> Vec<JournalRecord> {
    project
        .journal()
        .into_iter()
        .map(|e| e.record)
        .filter(|r| matches!(r, JournalRecord::JobFinished { .. }))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_declared_job_runs_between_sessions_and_the_next_one_gets_its_report() {
    let (_work, project, log, ran) = declaring(RENDER).await;
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
    assert_eq!(ran, ["sh render.sh"], "{log}");
    let journal = project.journal();
    assert!(
        journal.iter().any(|e| matches!(
            &e.record,
            JournalRecord::JobStarted { task, attempt: 1, command, minutes: 40 }
                if task.as_str() == "T002" && command == "sh render.sh"
        )),
        "{log}"
    );
    assert!(
        matches!(
            finished(&project).as_slice(),
            [JournalRecord::JobFinished { task, attempt: 1, exit_code: Some(0), timed_out: false, error: None, .. }]
                if task.as_str() == "T002"
        ),
        "{log}"
    );
    let prompts = builder_prompts(&project);
    assert_eq!(prompts.len(), 2, "{log}");
    assert!(!prompts[0].contains(REPORT), "{}", prompts[0]);
    assert!(
        prompts[1].contains(REPORT) && prompts[1].contains("ran: sh render.sh"),
        "{}",
        prompts[1]
    );
    assert!(project.read_if(".tenaz/jobs/T002.json").is_none());
}

/// A job file that does not stand runs nothing; the next session of the
/// attempt is told why, and the file is gone.
#[tokio::test(start_paused = true)]
async fn a_job_that_does_not_stand_runs_nothing() {
    let too_long = RENDER.replace("40", "600");
    let (_work, project, log, ran) = declaring(&too_long).await;
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
    assert!(ran.is_empty(), "{ran:?}");
    assert!(
        !project
            .journal()
            .iter()
            .any(|e| matches!(e.record, JournalRecord::JobStarted { .. })),
        "{log}"
    );
    let prompts = builder_prompts(&project);
    assert_eq!(prompts.len(), 2, "{log}");
    assert!(
        prompts[1].contains("Your .tenaz/jobs/T002.json does not stand, and nothing ran"),
        "{}",
        prompts[1]
    );
    assert!(project.read_if(".tenaz/jobs/T002.json").is_none());
}

/// A job that fails is reported, not judged: the attempt still ends on
/// `verify`.
#[tokio::test(start_paused = true)]
async fn a_failed_job_does_not_fail_the_attempt_by_itself() {
    let (_work, project, vcs) = project(&["maxAttempts=3"]);
    let jobs = Arc::new(ScriptedJobs::exiting(1));
    let log = play(&project, &vcs, Some(first_session_declares(RENDER)), jobs).await;
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
    assert!(
        matches!(
            finished(&project).as_slice(),
            [JournalRecord::JobFinished {
                exit_code: Some(1),
                ..
            }]
        ),
        "{log}"
    );
    assert!(
        builder_prompts(&project)[1].contains("`sh render.sh` exited with 1"),
        "{log}"
    );
}

/// A builder that declares a job in every session runs `jobsPerAttempt`
/// of them; the next file is removed unread and the attempt is judged.
#[tokio::test(start_paused = true)]
async fn an_attempt_runs_at_most_jobs_per_attempt_jobs() {
    let (_work, project, vcs) = project(&["maxAttempts=3", "jobsPerAttempt=2"]);
    let jobs = Arc::new(ScriptedJobs::default());
    let every = Some(ScriptedJob {
        task: "T002".into(),
        text: RENDER.into(),
        every_session: true,
    });
    let log = play(&project, &vcs, every, jobs).await;
    let in_attempt_1 = finished(&project)
        .iter()
        .filter(|r| matches!(r, JournalRecord::JobFinished { attempt: 1, .. }))
        .count();
    assert_eq!(in_attempt_1, 2, "{log}");
    assert!(
        log.contains("T002: .tenaz/jobs/T002.json ignored: the attempt has had 2 job files"),
        "{log}"
    );
    for number in [1, 2] {
        let path = format!(".tenaz/private/jobs/T002-1-{number}.log");
        assert!(project.read_if(&path).is_some(), "{path}");
    }
    assert!(project.read_if(".tenaz/jobs/T002.json").is_none());
}

/// A stop during the job pauses the run with the job file kept. On resume
/// the job runs before any session, and the first session gets its
/// report, without being told it goes on from a stopped session.
#[tokio::test(start_paused = true)]
async fn a_stop_during_the_job_pauses_and_the_job_runs_first_on_resume() {
    let (_work, project, vcs) = project(&["maxAttempts=3"]);
    let log = play(
        &project,
        &vcs,
        Some(first_session_declares(RENDER)),
        Arc::new(ScriptedJobs::stopped_during()),
    )
    .await;
    assert!(
        project.journal().iter().any(|e| matches!(
            &e.record,
            JournalRecord::Paused { label: Some(l), .. } if l == "T002's job"
        )),
        "{log}"
    );
    assert!(project.read_if(".tenaz/jobs/T002.json").is_some(), "{log}");
    assert!(finished(&project).is_empty(), "{log}");
    let before = builder_prompts(&project).len();

    // The resumed builder declares nothing: the kept file is what runs.
    let jobs = Arc::new(ScriptedJobs::default());
    let log = play(&project, &vcs, None, jobs.clone()).await;
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
    assert_eq!(jobs.ran(), ["sh render.sh"], "{log}");
    assert!(
        matches!(
            finished(&project).as_slice(),
            [JournalRecord::JobFinished {
                attempt: 1,
                exit_code: Some(0),
                ..
            }]
        ),
        "{log}"
    );
    let resumed = &builder_prompts(&project)[before];
    assert!(resumed.contains(REPORT), "{resumed}");
    assert!(!resumed.contains("was stopped because"), "{resumed}");
}

/// A stop as the job ends pauses before the session that reads its
/// report, and keeps the report: on resume that session gets it, and the
/// job does not run again.
#[tokio::test(start_paused = true)]
async fn a_stop_after_the_job_keeps_its_report_for_the_resumed_session() {
    let (_work, project, vcs) = project(&["maxAttempts=3"]);
    let first = Arc::new(ScriptedJobs::stopped_after());
    let log = play(
        &project,
        &vcs,
        Some(first_session_declares(RENDER)),
        first.clone(),
    )
    .await;
    assert_eq!(first.ran(), ["sh render.sh"], "{log}");
    assert!(project.read_if(".tenaz/jobs/T002.json").is_none(), "{log}");
    assert!(
        project.journal().iter().any(|e| matches!(
            &e.record,
            JournalRecord::Paused { label: Some(l), .. } if l == "T002's job"
        )),
        "{log}"
    );
    let jobs_of = |project: &Project| {
        project
            .load_ledger()
            .unwrap()
            .current
            .unwrap()
            .jobs
            .unwrap()
    };
    assert_eq!(
        jobs_of(&project).files,
        1,
        "the resumed attempt has had one job file"
    );
    let before = builder_prompts(&project).len();
    assert_eq!(before, 1, "a session ran after the stop:\n{log}");

    let jobs = Arc::new(ScriptedJobs::default());
    let log = play(&project, &vcs, None, jobs.clone()).await;
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
    assert!(jobs.ran().is_empty(), "{log}");
    let resumed = &builder_prompts(&project)[before];
    assert!(resumed.contains(REPORT), "{resumed}");
    assert!(!resumed.contains("was stopped because"), "{resumed}");
}

/// A job a stop cut short runs first on resume; a second stop as it ends
/// pauses again before any session, and the report waits for the next
/// resume.
#[tokio::test(start_paused = true)]
async fn a_stop_as_a_resumed_job_ends_pauses_before_any_session() {
    let (_work, project, vcs) = project(&["maxAttempts=3"]);
    let declared = Some(first_session_declares(RENDER));
    play(
        &project,
        &vcs,
        declared,
        Arc::new(ScriptedJobs::stopped_during()),
    )
    .await;
    let before = builder_prompts(&project).len();

    let rerun = Arc::new(ScriptedJobs::stopped_after());
    let log = play(&project, &vcs, None, rerun.clone()).await;
    assert_eq!(rerun.ran(), ["sh render.sh"], "{log}");
    assert_eq!(builder_prompts(&project).len(), before, "{log}");

    let jobs = Arc::new(ScriptedJobs::default());
    let log = play(&project, &vcs, None, jobs.clone()).await;
    assert!(jobs.ran().is_empty(), "{log}");
    let resumed = &builder_prompts(&project)[before];
    assert!(resumed.contains(REPORT), "{resumed}");
    assert!(!resumed.contains("was stopped because"), "{resumed}");
}

/// A builder session the engine failed for memory is rerun, not charged,
/// and the rerun is told to declare heavy work as a job.
#[tokio::test(start_paused = true)]
async fn a_builder_rerun_after_the_engine_ran_out_of_memory_is_told_to_declare_a_job() {
    let (_work, project, vcs) = project(&["maxAttempts=3"]);
    let harness = ScriptedHarness {
        out_of_memory_task: Some("T002".into()),
        ..Default::default()
    };
    let deps = deps(
        &project,
        vcs,
        Arc::new(harness),
        Arc::new(VirtualClock::new()),
    );
    run(&project, deps, CancellationToken::new()).await;
    let log = project.read(".tenaz/private/orchestrator.log").unwrap();
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
    assert!(log.contains("T002 done (1 attempt)"), "{log}");
    let prompts = builder_prompts(&project);
    assert_eq!(prompts.len(), 2, "{log}");
    assert!(!prompts[0].contains(OUT_OF_MEMORY), "{}", prompts[0]);
    assert!(
        prompts[1].trim_end().ends_with(OUT_OF_MEMORY),
        "{}",
        prompts[1]
    );
}

/// How long the fake LLM takes to load or unload: a large model takes
/// minutes, and none of it is the job's.
const ENGINE_SWAP: std::time::Duration = std::time::Duration::from_secs(90);

/// How long a job under the lease takes.
const JOB_TAKES: std::time::Duration = std::time::Duration::from_secs(30);

/// The fake goal's LLM, loaded at first, which refuses its next
/// `refusals` loads, as an engine short of memory after a job might.
struct ShortOfMemory {
    loaded: Mutex<bool>,
    refusals: Mutex<u32>,
}

impl ShortOfMemory {
    fn refusing(refusals: u32) -> Arc<Self> {
        Arc::new(Self {
            loaded: Mutex::new(true),
            refusals: Mutex::new(refusals),
        })
    }
}

#[async_trait]
impl EngineAdapter for ShortOfMemory {
    fn name(&self) -> &str {
        "short-of-memory"
    }

    async fn models(&self) -> Result<Vec<ModelState>, EngineError> {
        let loaded = *self.loaded.lock().unwrap();
        Ok(vec![ModelState {
            id: "fake".into(),
            loaded,
            ready: loaded,
            context_length: Some(131_072),
        }])
    }

    async fn load(&self, _model: &str) -> Result<(), EngineError> {
        tokio::time::sleep(ENGINE_SWAP).await;
        let mut refusals = self.refusals.lock().unwrap();
        if *refusals > 0 {
            *refusals -= 1;
            return Err(EngineError::Refused("no memory".into()));
        }
        *self.loaded.lock().unwrap() = true;
        Ok(())
    }

    async fn unload(&self, _model: &str) -> Result<(), EngineError> {
        tokio::time::sleep(ENGINE_SWAP).await;
        *self.loaded.lock().unwrap() = false;
        Ok(())
    }
}

/// The scripted harness, noting whether the lease's gate was open as each
/// session opened: a request of a session opened at a closed gate waits.
struct GateAtOpen {
    harness: ScriptedHarness,
    lease: Arc<LeaseManager>,
    seen: Mutex<Vec<(String, bool)>>,
}

#[async_trait]
impl Harness for GateAtOpen {
    fn info(&self) -> HarnessInfo {
        self.harness.info()
    }

    async fn open(&self, spec: SessionSpec) -> Result<Box<dyn Session>, HarnessError> {
        let open = self.lease.gate().is_open();
        self.seen.lock().unwrap().push((spec.label.clone(), open));
        self.harness.open(spec).await
    }
}

/// Plays the fake goal with T002's first builder declaring a job, under a
/// lease over `llm`; the project, its log and the gate as each session
/// opened.
async fn under_lease(
    llm: Arc<ShortOfMemory>,
) -> (tempfile::TempDir, Project, String, Vec<(String, bool)>) {
    under_lease_with(llm, Arc::new(VirtualClock::new()), CancellationToken::new()).await
}

/// [`under_lease`] with `clock` and `stop`.
async fn under_lease_with(
    llm: Arc<ShortOfMemory>,
    clock: Arc<dyn Clock>,
    stop: CancellationToken,
) -> (tempfile::TempDir, Project, String, Vec<(String, bool)>) {
    let (work, project, vcs) = project(&["maxAttempts=3"]);
    let lease = Arc::new(LeaseManager::new(
        llm,
        "fake",
        Gate::new(),
        Arc::new(|_| {}),
    ));
    let harness = Arc::new(GateAtOpen {
        harness: ScriptedHarness {
            job: Some(first_session_declares(RENDER)),
            ..Default::default()
        },
        lease: lease.clone(),
        seen: Mutex::new(Vec::new()),
    });
    let mut deps = deps(&project, vcs, harness.clone(), clock);
    deps.lease = Some(lease);
    deps.jobs = Arc::new(ScriptedJobs::taking(JOB_TAKES));
    run(&project, deps, stop).await;
    let log = project.read(".tenaz/private/orchestrator.log").unwrap();
    let seen = harness.seen.lock().unwrap().clone();
    (work, project, log, seen)
}

/// A reload that failed after the job leaves the gate closed; the LLM is
/// loaded and the gate opened before the next session opens. The report
/// names what the engine did.
#[tokio::test(start_paused = true)]
async fn the_llm_serves_again_before_the_session_after_a_job() {
    let (_work, project, log, seen) = under_lease(ShortOfMemory::refusing(1)).await;
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "{log}"
    );
    let builders: Vec<_> = seen
        .iter()
        .filter(|(label, _)| label.ends_with("-builder-T002"))
        .collect();
    assert_eq!(builders.len(), 2, "{seen:?}");
    assert!(
        builders[1].1,
        "the session after the job opened at a closed gate"
    );
    assert!(
        matches!(
            finished(&project).as_slice(),
            [JobFinished {
                exit_code: Some(0),
                engine: Some(_),
                duration_ms: 30_000,
                ..
            }]
        ),
        "{log}"
    );
    assert!(
        builder_prompts(&project)[1].contains("Around it, the engine: the LLM did not reload"),
        "{log}"
    );
}

/// An LLM that does not come back after the job blocks the run, with the
/// engine's error, the attempt given back and the report kept for the
/// session after a resume.
#[tokio::test(start_paused = true)]
async fn an_llm_that_does_not_serve_again_after_a_job_blocks_the_run() {
    let (_work, project, log, seen) = under_lease(ShortOfMemory::refusing(u32::MAX)).await;
    let ledger = project.load_ledger().unwrap();
    assert_eq!(ledger.phase, Phase::Blocked, "{log}");
    let reason = ledger.blocked_reason.clone().unwrap_or_default();
    assert!(
        reason.contains("the LLM did not serve again after T002's job")
            && reason.contains("no memory"),
        "{reason}"
    );
    assert!(
        log.contains("waiting for the LLM to serve again after T002's job"),
        "{log}"
    );
    let builders = seen
        .iter()
        .filter(|(label, _)| label.ends_with("-builder-T002"))
        .count();
    assert_eq!(builders, 1, "a session ran after the job: {seen:?}");
    let card = project.read(".tenaz/tasks/T002.md").unwrap();
    assert!(card.contains("attempts: 0"), "{card}");
    let jobs = ledger.current.unwrap().jobs.unwrap();
    assert!(jobs.report.is_some_and(|r| r.contains("sh render.sh")));
}

/// The virtual clock, except that the first wait for the engine stops the
/// run, as a person stopping it during that wait.
struct StopOnEngineWait {
    clock: VirtualClock,
    stop: CancellationToken,
}

#[async_trait]
impl Clock for StopOnEngineWait {
    fn now_rfc3339(&self) -> String {
        self.clock.now_rfc3339()
    }

    fn monotonic_ms(&self) -> u64 {
        self.clock.monotonic_ms()
    }

    async fn sleep(&self, duration: std::time::Duration) {
        if duration == std::time::Duration::from_secs(20) {
            self.stop.cancel();
        }
        self.clock.sleep(duration).await;
    }
}

/// A stop while the run waits for the LLM after a job pauses the run, with
/// the attempt given back and the report kept, rather than blocking it.
#[tokio::test(start_paused = true)]
async fn a_stop_while_waiting_for_the_llm_after_a_job_pauses_the_run() {
    let stop = CancellationToken::new();
    let clock = Arc::new(StopOnEngineWait {
        clock: VirtualClock::new(),
        stop: stop.clone(),
    });
    let (_work, project, log, _) =
        under_lease_with(ShortOfMemory::refusing(u32::MAX), clock, stop).await;
    let ledger = project.load_ledger().unwrap();
    assert_eq!(ledger.phase, Phase::Loop, "paused, not blocked:\n{log}");
    assert!(
        project.journal().iter().any(|e| matches!(
            &e.record,
            JournalRecord::Paused { label: Some(l), .. } if l == "T002's job"
        )),
        "{log}"
    );
    let card = project.read(".tenaz/tasks/T002.md").unwrap();
    assert!(card.contains("attempts: 0"), "{card}");
    let jobs = ledger.current.unwrap().jobs.unwrap();
    assert!(jobs.report.is_some_and(|r| r.contains("sh render.sh")));
}
