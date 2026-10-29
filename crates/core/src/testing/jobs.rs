//! The heavy-job runner for tests: no shell. Each job writes a line to its
//! log and ends with the exit code the test set (0 by default).

use std::sync::Mutex;

use async_trait::async_trait;

use crate::ports::{CancellationToken, JobOutcome, JobRunner, JobSpec};

#[derive(Debug, Default)]
pub struct ScriptedJobs {
    /// The exit code every job ends with.
    pub status: i32,
    ran: Mutex<Vec<String>>,
}

impl ScriptedJobs {
    /// The commands run so far, in order.
    pub fn ran(&self) -> Vec<String> {
        self.ran.lock().expect("lock").clone()
    }
}

#[async_trait]
impl JobRunner for ScriptedJobs {
    async fn run(&self, job: &JobSpec, stop: &CancellationToken) -> JobOutcome {
        self.ran.lock().expect("lock").push(job.command.clone());
        if stop.is_cancelled() {
            return JobOutcome {
                stopped: true,
                ..JobOutcome::default()
            };
        }
        let _ = std::fs::write(&job.log, format!("ran: {}\n", job.command));
        JobOutcome {
            status: Some(self.status),
            ..JobOutcome::default()
        }
    }
}
