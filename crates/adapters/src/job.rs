//! Runs the heavy jobs builders declare (ADR 0010): a shell command from
//! the project root, in a process group of its own, its output in a log
//! file, killed at its time limit or when the run stops.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tenaz_core::ports::{CancellationToken, JobOutcome, JobRunner, JobSpec};

/// `sh -c <command>` from the project root, through `wrapper` (the sandbox
/// launcher) when the run has one.
#[derive(Clone, Debug)]
pub struct JobProcess {
    root: PathBuf,
    wrapper: Vec<String>,
}

impl JobProcess {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            wrapper: Vec::new(),
        }
    }

    /// Starts the job through `wrapper` (the sandbox launcher).
    #[must_use]
    pub fn wrapped(mut self, wrapper: Vec<String>) -> Self {
        self.wrapper = wrapper;
        self
    }
}

#[async_trait]
impl JobRunner for JobProcess {
    async fn run(&self, job: &JobSpec, stop: &CancellationToken) -> JobOutcome {
        // A stop that came while the engine made room: a job started now
        // would only be killed, and a render may clear its output first.
        if stop.is_cancelled() {
            return JobOutcome {
                stopped: true,
                ..JobOutcome::default()
            };
        }
        let failed = |why: String| JobOutcome {
            error: Some(why),
            ..JobOutcome::default()
        };
        let log = match open_log(&job.log) {
            Ok(log) => log,
            Err(error) => return failed(format!("cannot open its log: {error}")),
        };
        let spawned = log.try_clone().map(|err| {
            crate::process::command(
                &self.root,
                &self.wrapper,
                Path::new("/bin/sh"),
                vec!["-c".to_owned(), job.command.clone()],
            )
            .stdout(log)
            .stderr(err)
            .spawn()
        });
        let mut child = match spawned {
            Ok(Ok(child)) => child,
            Ok(Err(error)) | Err(error) => {
                let why = format!("cannot run the job: {error}");
                let _ = std::fs::write(&job.log, format!("{why}\n"));
                return failed(why);
            }
        };
        let group = child.id();
        let outcome = tokio::select! {
            waited = tokio::time::timeout(job.timeout, child.wait()) => match waited {
                Ok(Ok(status)) => JobOutcome { status: status.code(), ..JobOutcome::default() },
                Ok(Err(error)) => failed(format!("cannot wait for the job: {error}")),
                Err(_) => JobOutcome { timed_out: true, ..JobOutcome::default() },
            },
            () = stop.cancelled() => JobOutcome { stopped: true, ..JobOutcome::default() },
        };
        // Whatever the job started goes with it: a render left running
        // would hold the GPU the LLM is about to reload into.
        crate::process::reap(&mut child, group).await;
        outcome
    }
}

/// The job's log, created with its directory.
fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::File::create(path)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::testing::gone_within;

    fn job() -> (tempfile::TempDir, JobProcess, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("jobs").join("job.log");
        let job = JobProcess::new(dir.path());
        (dir, job, log)
    }

    fn spec(command: &str, timeout: Duration, log: &Path) -> JobSpec {
        JobSpec {
            command: command.to_owned(),
            timeout,
            log: log.to_owned(),
        }
    }

    /// The log's directory is created; the job runs from the root.
    #[tokio::test]
    async fn a_job_runs_from_the_root_and_logs_its_output() {
        let (dir, job, log) = job();
        let outcome = job
            .run(
                &spec(
                    "echo out; echo err >&2; touch made; exit 3",
                    Duration::from_secs(5),
                    &log,
                ),
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(
            outcome,
            JobOutcome {
                status: Some(3),
                ..JobOutcome::default()
            }
        );
        assert!(dir.path().join("made").exists());
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains("out") && logged.contains("err"), "{logged}");
    }

    #[tokio::test]
    async fn a_job_past_its_time_limit_is_killed() {
        let (dir, job, log) = job();
        let pid = dir.path().join("pid");
        let command = format!("echo $$ > {}; exec sleep 30", pid.display());
        let outcome = job
            .run(
                &spec(&command, Duration::from_secs(1), &log),
                &CancellationToken::new(),
            )
            .await;
        assert!(outcome.timed_out && outcome.status.is_none(), "{outcome:?}");
        let pid = std::fs::read_to_string(&pid).unwrap();
        assert!(
            gone_within(pid.trim().parse().expect("a pid"), Duration::from_secs(5)),
            "the job still runs"
        );
    }

    /// A stopping run kills its job; no time has to pass for it.
    #[tokio::test]
    async fn a_job_stops_with_the_run() {
        let (dir, job, log) = job();
        let pid = dir.path().join("pid");
        let command = format!("echo $$ > {}; exec sleep 30", pid.display());
        let stop = CancellationToken::new();
        let stopping = stop.clone();
        let running = pid.clone();
        // Stop once the job has written its pid, the whole line: no fixed
        // delay, and no stop between creating the file and writing it.
        tokio::spawn(async move {
            while !std::fs::read_to_string(&running).is_ok_and(|p| p.ends_with('\n')) {
                tokio::task::yield_now().await;
            }
            stopping.cancel();
        });
        let outcome = job
            .run(&spec(&command, Duration::from_secs(30), &log), &stop)
            .await;
        assert!(outcome.stopped, "{outcome:?}");
        let pid = std::fs::read_to_string(&pid).unwrap();
        assert!(
            gone_within(pid.trim().parse().expect("a pid"), Duration::from_secs(5)),
            "the stopped job still runs"
        );
    }

    /// What the job left running in its group is killed with it.
    #[tokio::test]
    async fn what_a_job_started_in_the_background_is_killed() {
        let (dir, job, log) = job();
        let pid = dir.path().join("pid");
        let command = format!("sleep 30 & echo $! > {}; exit 0", pid.display());
        job.run(
            &spec(&command, Duration::from_secs(5), &log),
            &CancellationToken::new(),
        )
        .await;
        let pid = std::fs::read_to_string(&pid).unwrap();
        assert!(
            gone_within(pid.trim().parse().expect("a pid"), Duration::from_secs(5)),
            "the background sleep still runs"
        );
    }

    /// A job asked for after the run was stopped does not start.
    #[tokio::test]
    async fn a_job_after_the_stop_does_not_start() {
        let (dir, job, log) = job();
        let stop = CancellationToken::new();
        stop.cancel();
        let outcome = job
            .run(&spec("touch made", Duration::from_secs(5), &log), &stop)
            .await;
        assert_eq!(
            outcome,
            JobOutcome {
                stopped: true,
                ..JobOutcome::default()
            }
        );
        assert!(!dir.path().join("made").exists());
        assert!(!log.exists(), "its log was opened");
    }

    /// A job that cannot start says why, and so does its log.
    #[tokio::test]
    async fn a_job_that_cannot_start_says_why() {
        let (_dir, job, log) = job();
        let job = job.wrapped(vec!["/no/such/launcher".to_owned()]);
        let outcome = job
            .run(
                &spec("true", Duration::from_secs(5), &log),
                &CancellationToken::new(),
            )
            .await;
        let why = outcome.error.expect("a reason");
        assert!(why.starts_with("cannot run the job"), "{why}");
        assert!(std::fs::read_to_string(&log).unwrap().contains(&why));
    }
}
