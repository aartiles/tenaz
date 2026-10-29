//! A heavy job a builder declares (ADR 0010): what it asks to run, and the
//! report the next session of its attempt gets once it ran.

use std::fmt::Write as _;

use serde::Deserialize;

use crate::ports::JobOutcome;

/// `.tenaz/jobs/<TASK>.json`, as its builder wrote it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredJob {
    /// A shell command, run with `sh -c` from the project root.
    pub command: String,
    /// Its time limit.
    pub minutes: u64,
    /// Why it cannot run inside a session.
    pub why: String,
}

impl DeclaredJob {
    /// Reads a declared job; it may ask for at most `max_minutes`, which is
    /// at least 1: with jobs off, callers do not read the file.
    pub fn read(text: &str, max_minutes: u64) -> Result<Self, String> {
        debug_assert!(max_minutes > 0, "jobs are off: the file is not read");
        let job: Self = serde_json::from_str(text).map_err(|e| {
            format!("it is not {{\"command\": …, \"minutes\": …, \"why\": …}}: {e}")
        })?;
        if job.command.trim().is_empty() {
            return Err("its `command` is empty".to_owned());
        }
        if job.why.trim().is_empty() {
            return Err("its `why` is empty".to_owned());
        }
        if !(1..=max_minutes).contains(&job.minutes) {
            return Err(format!(
                "`minutes` is {}; a job may run 1 to {max_minutes} minutes (jobMinutes)",
                job.minutes
            ));
        }
        Ok(job)
    }
}

/// How a job that ran went, for the next session of its attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobRun<'a> {
    pub job: &'a DeclaredJob,
    pub outcome: &'a JobOutcome,
    pub elapsed_ms: u64,
    /// Where its log is, relative to the project root.
    pub log_path: &'a str,
    /// The end of its log, at most [`LOG_TAIL_BYTES`].
    pub log_tail: &'a str,
    /// What went wrong with the engine around it, if anything.
    pub engine: Option<&'a str>,
}

/// The bytes of a job's log shown in the report.
pub const LOG_TAIL_BYTES: usize = 2000;

impl JobRun<'_> {
    /// The report: how it ended, how long it took, and the end of its log.
    pub fn report(&self) -> String {
        let ended = match (self.outcome.status, &self.outcome.error) {
            (_, Some(why)) => format!("did not run: {why}"),
            _ if self.outcome.timed_out => {
                format!("was stopped at its limit of {} minutes", self.job.minutes)
            }
            (Some(code), None) => format!("exited with {code}"),
            (None, None) => "was killed by a signal (out of memory?)".to_owned(),
        };
        let mut report = format!(
            "`{}` {ended} after {}. Its log is {}.",
            self.job.command,
            duration(self.elapsed_ms),
            self.log_path
        );
        if let Some(engine) = self.engine {
            let _ = write!(report, " Around it, the engine: {engine}.");
        }
        let tail = self.log_tail.trim_end();
        if !tail.is_empty() {
            let _ = write!(report, "\n\nThe end of its log:\n\n```text\n{tail}\n```");
        }
        report
    }
}

/// `1h 02m`, `12m 03s` or `45s`.
fn duration(ms: u64) -> String {
    let s = ms / 1000;
    match (s / 3600, (s / 60) % 60, s % 60) {
        (0, 0, sec) => format!("{sec}s"),
        (0, min, sec) => format!("{min}m {sec:02}s"),
        (hours, min, _) => format!("{hours}h {min:02}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RENDER: &str = r#"{"command": "sh render/loop.sh", "minutes": 40, "why": "180 Cycles frames take 25 minutes"}"#;

    fn render() -> DeclaredJob {
        DeclaredJob {
            command: "sh render/loop.sh".to_owned(),
            minutes: 40,
            why: "180 Cycles frames take 25 minutes".to_owned(),
        }
    }

    #[test]
    fn a_job_names_its_command_time_and_reason() {
        assert_eq!(DeclaredJob::read(RENDER, 60).unwrap(), render());
    }

    #[test]
    fn a_job_that_asks_too_much_or_too_little_does_not_stand() {
        for (text, why) in [
            (RENDER.replace("40", "90"), "1 to 60 minutes"),
            (RENDER.replace("40", "0"), "1 to 60 minutes"),
            (
                RENDER.replace("sh render/loop.sh", " "),
                "`command` is empty",
            ),
            (
                RENDER.replace("180 Cycles frames take 25 minutes", ""),
                "`why` is empty",
            ),
            ("not json".to_owned(), "it is not"),
            (RENDER.replace("\"why\"", "\"reason\""), "it is not"),
            (RENDER.replace('}', r#", "extra": 1}"#), "unknown field"),
        ] {
            let error = DeclaredJob::read(&text, 60).unwrap_err();
            assert!(error.contains(why), "{text}: {error}");
        }
    }

    fn run<'a>(job: &'a DeclaredJob, outcome: &'a JobOutcome, tail: &'a str) -> JobRun<'a> {
        JobRun {
            job,
            outcome,
            elapsed_ms: 1_503_000,
            log_path: ".tenaz/private/jobs/T017-1-1.log",
            log_tail: tail,
            engine: None,
        }
    }

    #[test]
    fn the_report_says_how_the_job_ended_and_shows_the_end_of_its_log() {
        let job = render();
        let outcome = JobOutcome {
            status: Some(0),
            ..JobOutcome::default()
        };
        assert_eq!(
            run(&job, &outcome, "frame 180/180\n").report(),
            "`sh render/loop.sh` exited with 0 after 25m 03s. Its log is \
             .tenaz/private/jobs/T017-1-1.log.\n\nThe end of its log:\n\n```text\nframe 180/180\n```"
        );
    }

    #[test]
    fn the_report_names_each_way_a_job_ends() {
        let job = render();
        let error = Some("cannot run the job: no sh".to_owned());
        for (outcome, ended) in [
            (
                JobOutcome {
                    status: Some(1),
                    ..JobOutcome::default()
                },
                "exited with 1",
            ),
            (
                JobOutcome {
                    timed_out: true,
                    ..JobOutcome::default()
                },
                "was stopped at its limit of 40 minutes",
            ),
            (
                JobOutcome {
                    error: error.clone(),
                    ..JobOutcome::default()
                },
                "did not run: cannot run the job: no sh",
            ),
            (
                JobOutcome {
                    error,
                    timed_out: true,
                    ..JobOutcome::default()
                },
                "did not run: cannot run the job: no sh",
            ),
            (
                JobOutcome::default(),
                "was killed by a signal (out of memory?)",
            ),
        ] {
            let report = run(&job, &outcome, "").report();
            assert!(
                report.starts_with(&format!("`sh render/loop.sh` {ended} after")),
                "{report}"
            );
        }
    }

    #[test]
    fn the_report_names_what_went_wrong_with_the_engine() {
        let job = render();
        let ok = JobOutcome {
            status: Some(0),
            ..JobOutcome::default()
        };
        let report = JobRun {
            engine: Some("the LLM did not reload after a job: refused"),
            ..run(&job, &ok, "")
        }
        .report();
        assert!(
            report.ends_with("Around it, the engine: the LLM did not reload after a job: refused."),
            "{report}"
        );
    }

    #[test]
    fn durations_read_as_a_person_would_say_them() {
        assert_eq!(duration(45_000), "45s");
        assert_eq!(duration(723_000), "12m 03s");
        assert_eq!(duration(3_720_000), "1h 02m");
    }
}
