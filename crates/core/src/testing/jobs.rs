//! The heavy-job runner for tests: no shell. Each job writes a line to its
//! log and ends with the exit code the test set (0 by default).

use std::sync::Mutex;

use async_trait::async_trait;

use crate::ports::{CancellationToken, JobOutcome, JobRunner, JobSpec};

#[derive(Debug, Default)]
pub struct ScriptedJobs {
    /// The exit code every job ends with.
    status: i32,
    /// How long every job takes, on tokio's clock.
    takes: std::time::Duration,
    stop: Option<Stop>,
    ran: Mutex<Vec<String>>,
}

/// When the first job stops the run, as a person stopping it would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    /// While it runs: the job is cut short.
    During,
    /// Once it has ended: the session after it is.
    After,
}

impl ScriptedJobs {
    /// Jobs that each end with `status`.
    pub fn exiting(status: i32) -> Self {
        Self {
            status,
            ..Self::default()
        }
    }

    /// Jobs that each take `takes`.
    pub fn taking(takes: std::time::Duration) -> Self {
        Self {
            takes,
            ..Self::default()
        }
    }

    /// Jobs whose first one is stopped while it runs.
    pub fn stopped_during() -> Self {
        Self {
            stop: Some(Stop::During),
            ..Self::default()
        }
    }

    /// Jobs whose first one ends, and the run is stopped right after it.
    pub fn stopped_after() -> Self {
        Self {
            stop: Some(Stop::After),
            ..Self::default()
        }
    }

    /// The commands run so far, in order.
    pub fn ran(&self) -> Vec<String> {
        self.ran.lock().expect("lock").clone()
    }
}

#[async_trait]
impl JobRunner for ScriptedJobs {
    async fn run(&self, job: &JobSpec, stop: &CancellationToken) -> JobOutcome {
        let first = {
            let mut ran = self.ran.lock().expect("lock");
            ran.push(job.command.clone());
            ran.len() == 1
        };
        let stops = |when| first && self.stop == Some(when);
        if stops(Stop::During) {
            stop.cancel();
        }
        if stop.is_cancelled() {
            return JobOutcome {
                stopped: true,
                ..JobOutcome::default()
            };
        }
        tokio::time::sleep(self.takes).await;
        // As the real runner does, the log's folder is made if missing.
        if let Some(dir) = job.log.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&job.log, format!("ran: {}\n", job.command));
        if stops(Stop::After) {
            stop.cancel();
        }
        JobOutcome {
            status: Some(self.status),
            ..JobOutcome::default()
        }
    }
}
