//! A heavy job a builder declared (ADR 0010): run between the sessions of
//! its attempt, with the engine's memory to itself, and reported to the
//! session that follows.

use std::time::Duration;

use crate::backlog::Backlog;
use crate::job::{DeclaredJob, JobRun, LOG_TAIL_BYTES};
use crate::lease::JobLease;
use crate::ledger::{AttemptJobs, JournalRecord};
use crate::orchestrator::session::{SessionCtx, SessionResult};
use crate::orchestrator::{RunError, Runner};
use crate::ports::{JobOutcome, JobSpec};
use crate::project::job_path;
use crate::role::Role;
use crate::task::TaskId;

use super::task::LoadedCard;

/// The title of the section that tells the next session how its job went.
const JOB_TITLE: &str = "The job you declared";

impl Runner {
    /// The builder sessions of `attempt`: one, and after each job its
    /// builder declares, a fresh one told how it went. A job a stop left
    /// runs first, and a report no session has read yet goes to the first
    /// session: the job file and the ledger keep both across a pause.
    pub(super) async fn builder_sessions(
        &mut self,
        backlog: &Backlog,
        loaded: &LoadedCard,
        attempt: u32,
    ) -> Result<SessionResult, RunError> {
        let task = &loaded.card.id;
        let mut ran = self.next_job(task, attempt).await?;
        loop {
            // A stop that came as a job ended pauses before the next
            // session; the job's report waits in the ledger.
            if ran && self.stopping() {
                return Err(self.pause_job(task, attempt)?);
            }
            let prompt = self.builder_prompt(backlog, loaded, attempt).await?;
            let session = self.builder_session(&loaded.card, attempt, &prompt).await?;
            if let Some(jobs) = self.ledger.current.as_mut().and_then(|c| c.jobs.as_mut())
                && jobs.report.take().is_some()
            {
                self.save()?;
            }
            ran = self.next_job(task, attempt).await?;
            if !ran {
                return Ok(session);
            }
        }
    }

    /// The section on the job of `attempt` no session has read yet, if any.
    pub(super) fn job_section(&self, attempt: u32) -> Option<(String, String)> {
        let jobs = self.ledger.current.as_ref()?.jobs.as_ref()?;
        let report = jobs.report.as_ref().filter(|_| jobs.attempt == attempt)?;
        Some((JOB_TITLE.to_owned(), report.clone()))
    }

    /// The jobs of `attempt` of the current task; a new attempt starts
    /// with none. Only builder attempts call it, and a task's attempts run
    /// with `current` set.
    fn attempt_jobs(&mut self, attempt: u32) -> &mut AttemptJobs {
        let current = self
            .ledger
            .current
            .as_mut()
            .expect("a builder attempt has a current task");
        let jobs = current
            .jobs
            .get_or_insert_with(|| AttemptJobs::new(attempt));
        if jobs.attempt != attempt {
            *jobs = AttemptJobs::new(attempt);
        }
        jobs
    }

    /// Runs the job `task`'s builder declared, if any, and keeps its report
    /// for the next session; true when there is one. A job file that does
    /// not stand runs nothing, and the report says why. Past
    /// `jobsPerAttempt` job files, the file is removed and the attempt is
    /// judged as it stands.
    async fn next_job(&mut self, task: &TaskId, attempt: u32) -> Result<bool, RunError> {
        let path = job_path(task.as_str());
        let Some(text) = self.project.read_if(&path) else {
            return Ok(false);
        };
        let files = self.attempt_jobs(attempt).files;
        if files >= self.params.jobs_per_attempt {
            self.log(&format!(
                "  {task}: {path} ignored: the attempt has had {files} job files (jobsPerAttempt)"
            ));
            self.project.remove(&path)?;
            return Ok(false);
        }
        let report = match DeclaredJob::read(&text, self.params.job_minutes) {
            Ok(job) => self.run_job(task, attempt, &job).await?,
            Err(why) => {
                self.log(&format!("  {task}: {path} does not stand: {why}"));
                format!("Your {path} does not stand, and nothing ran: {why}.")
            }
        };
        // The report is saved before the file goes: a run that dies in
        // between runs the job again rather than losing what it found.
        let jobs = self.attempt_jobs(attempt);
        jobs.files += 1;
        jobs.report = Some(report);
        self.save()?;
        self.project.remove(&path)?;
        Ok(true)
    }

    /// Runs `job`, the attempt's next, under the lease and journals it. A
    /// stop during the job pauses the run with the job file kept: on resume
    /// the job runs again before any session.
    async fn run_job(
        &mut self,
        task: &TaskId,
        attempt: u32,
        job: &DeclaredJob,
    ) -> Result<String, RunError> {
        let number = self.attempt_jobs(attempt).files + 1;
        let log_path = format!(".tenaz/private/jobs/{task}-{attempt}-{number}.log");
        let spec = JobSpec {
            command: job.command.clone(),
            timeout: Duration::from_secs(job.minutes.saturating_mul(60)),
            log: self.project.path(&log_path),
        };
        self.journal(JournalRecord::JobStarted {
            task: task.clone(),
            attempt,
            command: job.command.clone(),
            minutes: job.minutes,
        });
        self.log(&format!(
            "  {task}: running the job its builder declared, with the engine's memory to itself (limit {} min): {}",
            job.minutes, job.command
        ));
        let started = self.deps.clock.monotonic_ms();
        let (outcome, engine) = self.run_under_lease(&spec).await;
        if outcome.stopped {
            return Err(self.pause_job(task, attempt)?);
        }
        let elapsed_ms = self.deps.clock.monotonic_ms().saturating_sub(started);
        self.journal(JournalRecord::JobFinished {
            task: task.clone(),
            attempt,
            exit_code: outcome.status,
            timed_out: outcome.timed_out,
            error: outcome.error.clone(),
            engine: engine.clone(),
            duration_ms: elapsed_ms,
        });
        let log_tail = self
            .project
            .tail(&log_path, LOG_TAIL_BYTES)
            .unwrap_or_default();
        let report = JobRun {
            job,
            outcome: &outcome,
            elapsed_ms,
            log_path: &log_path,
            log_tail: &log_tail,
            engine: engine.as_deref(),
        }
        .report();
        self.log(&format!(
            "  {task}: {}",
            report.lines().next().unwrap_or_default()
        ));
        Ok(report)
    }

    /// Pauses the run during `task`'s job. The attempt is given back, as
    /// for a stopped session, but the next session is not told it goes on
    /// from a stopped one: none was.
    fn pause_job(&mut self, task: &TaskId, attempt: u32) -> Result<RunError, RunError> {
        self.project.set_attempts(task, attempt - 1)?;
        let ctx = SessionCtx {
            task: Some(task.clone()),
            attempt: Some(attempt),
            ..SessionCtx::default()
        };
        self.record_pause(&format!("{task}'s job"), Role::Builder, &ctx)
    }

    /// The job's outcome, run with the LLM away when the run has a lease,
    /// and what went wrong with the engine around it. A job that never ran
    /// while a stop came is stopped, so it runs on resume; one that ran
    /// keeps its outcome.
    async fn run_under_lease(&self, spec: &JobSpec) -> (JobOutcome, Option<String>) {
        let job = self.deps.jobs.run(spec, &self.stop);
        let Some(lease) = &self.deps.lease else {
            return (job.await, None);
        };
        let ran = lease.run_job(job).await;
        if ran.output.is_none() && self.stopping() {
            let stopped = JobOutcome {
                stopped: true,
                ..JobOutcome::default()
            };
            return (stopped, None);
        }
        leased_outcome(ran)
    }
}

/// A job's outcome from how the lease ran it, and what went wrong with the
/// engine around it. A job the engine did not free its memory for never
/// ran.
fn leased_outcome(ran: JobLease<JobOutcome>) -> (JobOutcome, Option<String>) {
    let engine = ran.lease.err().map(|e| e.to_string());
    let outcome = ran.output.unwrap_or_else(|| JobOutcome {
        error: Some("the engine could not free its memory, so it did not run".to_owned()),
        ..JobOutcome::default()
    });
    (outcome, engine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::LeaseError;
    use crate::ports::EngineError;

    fn exited(code: i32) -> JobOutcome {
        JobOutcome {
            status: Some(code),
            ..JobOutcome::default()
        }
    }

    #[test]
    fn a_job_the_lease_ran_keeps_its_outcome() {
        let ran = JobLease {
            output: Some(exited(0)),
            lease: Ok(()),
        };
        assert_eq!(leased_outcome(ran), (exited(0), None));
    }

    #[test]
    fn an_llm_that_did_not_reload_is_named_beside_the_outcome() {
        let ran = JobLease {
            output: Some(exited(0)),
            lease: Err(LeaseError::ReloadFailed(EngineError::Refused(
                "busy".to_owned(),
            ))),
        };
        assert_eq!(
            leased_outcome(ran),
            (
                exited(0),
                Some("the LLM did not reload after a job: engine refused: busy".to_owned())
            )
        );
    }

    #[test]
    fn a_job_the_engine_did_not_make_room_for_did_not_run() {
        let ran = JobLease {
            output: None,
            lease: Err(LeaseError::Engine(EngineError::Timeout(
                "unload".to_owned(),
            ))),
        };
        let (outcome, engine) = leased_outcome(ran);
        assert_eq!(
            outcome.error.as_deref(),
            Some("the engine could not free its memory, so it did not run")
        );
        assert_eq!(engine.as_deref(), Some("engine timed out: unload"));
    }
}
