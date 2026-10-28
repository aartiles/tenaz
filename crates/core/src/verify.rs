//! The `verify` contract the orchestrator enforces: an executable (`verify`
//! at the project root, or `.tenaz/verify` in an existing project), exit code
//! 0 on success, and one JSON object as the last line of stdout.

use serde::{Deserialize, Serialize};

use crate::util::{clip, strip_ansi};

/// One failed check in the report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyFailure {
    pub id: String,
    #[serde(default)]
    pub msg: String,
}

/// The JSON last line of `verify`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VerifyReport {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub passed: u64,
    #[serde(default)]
    pub failed: Vec<VerifyFailure>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
}

/// What running `verify` produced, before interpretation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawVerifyOutput {
    /// The executable does not exist at the project root.
    pub missing: bool,
    pub timed_out: bool,
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// The interpreted result: pass or fail, the parsed report when the last line
/// was JSON, and a summary clipped for a model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerifyOutcome {
    pub ok: bool,
    pub exit: Option<i32>,
    pub json: Option<serde_json::Value>,
    pub summary: String,
}

/// Why an attempt at `task` fails when the checks of other tasks that
/// `./verify --all` passes (`passed` in all, less `own` with the task's
/// prefix) are fewer than at the last checkpoint: one was removed, or
/// `verify` no longer runs it. The task's own checks are new since then, so
/// they cannot make up for a removed one.
fn fewer_checks(task: &str, passed: u64, own: u64, floor: Option<u64>) -> Option<String> {
    let floor = floor?;
    let others = passed.saturating_sub(own);
    (others < floor).then(|| {
        format!(
            "./verify --all passes {others} checks besides {task}'s, fewer than the {floor} that passed when the last task was done: a check was removed or is no longer run. Put it back; if a check is wrong, say so in your notes instead."
        )
    })
}

/// Why an attempt at `task` fails, judged by `./verify --all` (`all`) and
/// `./verify --task` (`own`) after it; `None` when it passed. The whole
/// suite must pass, keep the `floor` of other tasks' checks (see
/// `fewer_checks`), and run at least one passing check of the task.
pub fn judge_attempt(
    task: &str,
    all: &VerifyOutcome,
    own: &VerifyOutcome,
    floor: Option<u64>,
) -> Option<String> {
    if !all.ok {
        return Some(format!(
            "./verify --all fails after the attempt:\n{}",
            all.summary
        ));
    }
    let own_checks = own.passed();
    if let Some(why) = fewer_checks(task, all.passed(), own_checks, floor) {
        return Some(why);
    }
    (!own.ok || own_checks < 1).then(|| {
        format!(
            "./verify --task {task} must run at least one passing check with ID prefix \"{task}.\" (got {own_checks}).\n{}",
            own.summary
        )
    })
}

/// An attempt's `failure` (see `judge_attempt`) after the orchestrator
/// stopped its session: why it was `stopped` comes first, so the next
/// attempt knows the session did not end on its own.
pub fn with_stop_reason(failure: Option<String>, stopped: Option<&str>) -> Option<String> {
    failure.map(|failure| match stopped {
        Some(stopped) => format!("The session was stopped: {stopped}.\n{failure}"),
        None => failure,
    })
}

impl VerifyOutcome {
    /// The number of passed checks, 0 without a report.
    pub fn passed(&self) -> u64 {
        self.json
            .as_ref()
            .and_then(|j| j.get("passed"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    }

    pub fn report(&self) -> Option<VerifyReport> {
        self.json
            .clone()
            .and_then(|j| serde_json::from_value(j).ok())
    }
}

/// The JSON object that ends `stdout`: its last line, or, when a model
/// printed it indented, the object that starts on a line of its own and runs
/// to the end.
fn final_json(stdout: &str) -> Option<serde_json::Value> {
    let text = stdout.trim_end();
    let object = |s: &str| {
        serde_json::from_str::<serde_json::Value>(s)
            .ok()
            .filter(serde_json::Value::is_object)
    };
    if let Some(json) = object(text.lines().last().unwrap_or("")) {
        return Some(json);
    }
    text.match_indices('\n')
        .map(|(i, _)| i + 1)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .chain(std::iter::once(0))
        .filter(|&i| text[i..].starts_with('{'))
        .find_map(|i| object(&text[i..]))
}

/// Interprets a `verify` run as the task loop did.
pub fn interpret(
    args: &[&str],
    raw: &RawVerifyOutput,
    timeout_sec: u64,
    clip_bytes: usize,
) -> VerifyOutcome {
    let argv = args.join(" ");
    if raw.missing {
        return VerifyOutcome {
            ok: false,
            exit: None,
            json: None,
            summary: "verify does not exist at the project root".to_owned(),
        };
    }
    let stdout = strip_ansi(&raw.stdout);
    let mut parsed = final_json(&stdout);
    // A check's message often quotes coloured tool output; the JSON keeps
    // the escapes as `\u001b`, so they survive parsing.
    if let Some(failed) = parsed
        .as_mut()
        .and_then(|j| j.get_mut("failed"))
        .and_then(serde_json::Value::as_array_mut)
    {
        for msg in failed.iter_mut().filter_map(|f| f.get_mut("msg")) {
            if let Some(text) = msg.as_str() {
                *msg = strip_ansi(text).into();
            }
        }
    }
    let ok = raw.status == Some(0)
        && parsed
            .as_ref()
            .and_then(|j| j.get("ok"))
            .and_then(serde_json::Value::as_bool)
            == Some(true);
    let summary = if raw.timed_out {
        format!("verify {argv} timed out after {timeout_sec}s")
    } else if let Some(json) = &parsed {
        clip(
            &format!("verify {argv} -> exit {}\n{json}", exit_text(raw.status)),
            clip_bytes,
        )
    } else {
        format!(
            "verify {argv} exited {} without a JSON last line.\nstdout tail:\n{}\nstderr tail:\n{}",
            exit_text(raw.status),
            tail(&stdout, 2000),
            tail(&strip_ansi(&raw.stderr), 1500)
        )
    };
    VerifyOutcome {
        ok,
        exit: raw.status,
        json: parsed,
        summary,
    }
}

fn exit_text(status: Option<i32>) -> String {
    status.map_or_else(|| "null".to_owned(), |s| s.to_string())
}

fn tail(text: &str, bytes: usize) -> String {
    if text.len() <= bytes {
        return text.to_owned();
    }
    let start = text.ceil_char_boundary(text.len() - bytes);
    text[start..].to_owned()
}

/// Text for a session, with `./verify` and `` `verify` `` pointing at `path`.
/// The prompts and steers name the default `verify` at the root; an existing
/// project keeps it at `.tenaz/verify` instead.
pub fn retarget(text: &str, path: &str) -> String {
    if path == "verify" {
        return text.to_owned();
    }
    text.replace("./verify", &format!("./{path}"))
        .replace("`verify`", &format!("`{path}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fewer_passing_checks_than_the_last_checkpoint_fail() {
        assert_eq!(fewer_checks("T004", 5, 0, None), None);
        assert_eq!(fewer_checks("T004", 7, 2, Some(5)), None);
        assert_eq!(fewer_checks("T004", 6, 0, Some(5)), None);
        // One earlier check removed and one of the task's added: still fewer.
        let why = fewer_checks("T004", 5, 1, Some(5)).unwrap();
        assert!(
            why.contains("passes 4 checks besides T004's, fewer than the 5"),
            "{why}"
        );
    }

    /// What a real `./verify` run gives: `report` as its last line.
    fn run(args: &[&str], status: i32, report: &str) -> VerifyOutcome {
        interpret(args, &raw(status, &format!("{report}\n")), 60, 4000)
    }

    fn passing(args: &[&str], passed: u64) -> VerifyOutcome {
        run(
            args,
            0,
            &format!(r#"{{"ok":true,"passed":{passed},"failed":[]}}"#),
        )
    }

    fn failing_suite(passed: u64) -> VerifyOutcome {
        run(
            &["--all"],
            1,
            &format!(r#"{{"ok":false,"passed":{passed},"failed":[{{"id":"T001.a"}}]}}"#),
        )
    }

    const ALL: &[&str] = &["--all"];
    const OWN: &[&str] = &["--task", "T004"];
    const FEWER: &str = "passes 4 checks besides T004's, fewer than the 5";

    #[test]
    fn an_attempt_that_keeps_every_check_and_adds_its_own_passes() {
        let all = passing(ALL, 6);
        let own = passing(OWN, 1);
        assert_eq!(judge_attempt("T004", &all, &own, Some(5)), None);
    }

    /// The suite's failure is named, with its summary, whatever else
    /// fails: nothing else, a removed check, or a missing own check.
    #[test]
    fn a_failing_suite_fails_the_attempt_first_with_its_summary() {
        let all = failing_suite(0);
        let expected = format!("./verify --all fails after the attempt:\n{}", all.summary);
        for (own, floor) in [
            (passing(OWN, 1), None),
            (passing(OWN, 1), Some(5)),
            (passing(OWN, 0), None),
        ] {
            assert_eq!(
                judge_attempt("T004", &all, &own, floor).as_deref(),
                Some(expected.as_str()),
                "own {own:?}, floor {floor:?}"
            );
        }
    }

    #[test]
    fn a_stopped_session_s_failure_says_why_it_was_stopped_first() {
        assert_eq!(
            with_stop_reason(Some("verify fails".to_owned()), Some("loop")).as_deref(),
            Some("The session was stopped: loop.\nverify fails")
        );
    }

    #[test]
    fn a_failure_of_a_session_that_was_not_stopped_is_kept() {
        assert_eq!(
            with_stop_reason(Some("verify fails".to_owned()), None).as_deref(),
            Some("verify fails")
        );
    }

    #[test]
    fn a_stopped_session_whose_work_passes_has_no_failure() {
        assert_eq!(with_stop_reason(None, Some("loop")), None);
    }

    #[test]
    fn a_missing_verify_fails_the_attempt() {
        let missing = RawVerifyOutput {
            missing: true,
            ..Default::default()
        };
        let all = interpret(ALL, &missing, 60, 4000);
        assert_eq!(
            judge_attempt("T004", &all, &passing(OWN, 1), None).as_deref(),
            Some(
                "./verify --all fails after the attempt:\nverify does not exist at the project root"
            )
        );
    }

    #[test]
    fn a_removed_check_fails_the_attempt() {
        let why = judge_attempt("T004", &passing(ALL, 5), &passing(OWN, 1), Some(5)).unwrap();
        assert!(why.contains(FEWER), "{why}");
    }

    #[test]
    fn a_removed_check_is_named_before_a_missing_own_check() {
        let why = judge_attempt("T004", &passing(ALL, 4), &passing(OWN, 0), Some(5)).unwrap();
        assert!(why.contains(FEWER), "{why}");
        assert!(
            !why.contains("must run at least one passing check"),
            "{why}"
        );
    }

    #[test]
    fn an_attempt_without_a_passing_check_of_its_own_fails() {
        let own = passing(OWN, 0);
        assert_eq!(
            judge_attempt("T004", &passing(ALL, 5), &own, Some(5)),
            Some(format!(
                "./verify --task T004 must run at least one passing check with ID prefix \"T004.\" (got 0).\n{}",
                own.summary
            ))
        );
    }

    #[test]
    fn a_task_run_that_timed_out_fails_the_attempt() {
        let timed_out = RawVerifyOutput {
            timed_out: true,
            ..Default::default()
        };
        let own = interpret(OWN, &timed_out, 60, 4000);
        assert_eq!(
            judge_attempt("T004", &passing(ALL, 5), &own, Some(5)).as_deref(),
            Some(
                "./verify --task T004 must run at least one passing check with ID prefix \"T004.\" (got 0).\nverify --task T004 timed out after 60s"
            )
        );
    }

    #[test]
    fn only_a_checkpoint_sets_a_floor_of_other_checks() {
        let (all, own) = (passing(ALL, 1), passing(OWN, 1));
        assert_eq!(judge_attempt("T004", &all, &own, None), None);
        let why = judge_attempt("T004", &all, &own, Some(5)).unwrap();
        assert!(
            why.contains("passes 0 checks besides T004's, fewer than the 5"),
            "{why}"
        );
    }

    #[test]
    fn retarget_points_the_text_at_the_verify_path() {
        let text = "Run ./verify --all. If you need a check, add it to `verify`.";
        assert_eq!(retarget(text, "verify"), text);
        assert_eq!(
            retarget(text, ".tenaz/verify"),
            "Run ./.tenaz/verify --all. If you need a check, add it to `.tenaz/verify`."
        );
    }

    fn raw(status: i32, stdout: &str) -> RawVerifyOutput {
        RawVerifyOutput {
            status: Some(status),
            stdout: stdout.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn passing_run_needs_exit_zero_and_ok_true() {
        let out = interpret(
            &["--all"],
            &raw(0, "progress\n{\"ok\":true,\"passed\":3,\"failed\":[]}\n"),
            120,
            4000,
        );
        assert!(out.ok);
        assert_eq!(out.passed(), 3);
        assert_eq!(
            out.summary,
            "verify --all -> exit 0\n{\"ok\":true,\"passed\":3,\"failed\":[]}"
        );
        assert_eq!(out.report().unwrap().passed, 3);
        assert!(
            !interpret(
                &["--all"],
                &raw(1, "{\"ok\":true,\"passed\":3,\"failed\":[]}"),
                120,
                4000
            )
            .ok
        );
        assert!(
            !interpret(
                &["--all"],
                &raw(0, "{\"ok\":false,\"passed\":3,\"failed\":[]}"),
                120,
                4000
            )
            .ok
        );
    }

    #[test]
    fn missing_executable_timeouts_and_non_json_are_explained() {
        let missing = interpret(
            &["--all"],
            &RawVerifyOutput {
                missing: true,
                ..Default::default()
            },
            120,
            4000,
        );
        assert_eq!(missing.summary, "verify does not exist at the project root");
        let timeout = interpret(
            &["--task", "T007"],
            &RawVerifyOutput {
                timed_out: true,
                status: None,
                ..Default::default()
            },
            90,
            4000,
        );
        assert_eq!(timeout.summary, "verify --task T007 timed out after 90s");
        assert!(!timeout.ok);
        let text = interpret(
            &["--all"],
            &RawVerifyOutput {
                status: Some(2),
                stdout: "boom".into(),
                stderr: "trace".into(),
                ..Default::default()
            },
            120,
            4000,
        );
        assert_eq!(
            text.summary,
            "verify --all exited 2 without a JSON last line.\nstdout tail:\nboom\nstderr tail:\ntrace"
        );
        assert_eq!(text.json, None);
    }

    #[test]
    fn colour_codes_are_left_out_of_messages_and_tails() {
        let stdout = "\u{1b}[32mok\u{1b}[39m\n{\"ok\":false,\"passed\":3,\"failed\":[{\"id\":\"T001.dev-serves\",\"msg\":\"\\u001b[1mNetwork\\u001b[22m: use --host\"}]}\n";
        let out = interpret(&["--all"], &raw(1, stdout), 120, 4000);
        assert_eq!(out.report().unwrap().failed[0].msg, "Network: use --host");
        assert!(!out.summary.contains("\\u001b"), "{}", out.summary);
        let text = interpret(
            &["--all"],
            &RawVerifyOutput {
                status: Some(1),
                stdout: "\u{1b}[31mboom\u{1b}[0m".into(),
                stderr: "\u{1b}[2mtrace\u{1b}[22m".into(),
                ..Default::default()
            },
            120,
            4000,
        );
        assert!(
            text.summary
                .ends_with("stdout tail:\nboom\nstderr tail:\ntrace")
        );
    }

    #[test]
    fn an_indented_final_object_counts_as_the_report() {
        let stdout = "checking\n{\"id\": \"T001.a\", \"status\": \"ok\"}\n{\n  \"ok\": true,\n  \"passed\": 4,\n  \"failed\": []\n}\n";
        let out = interpret(&["--all"], &raw(0, stdout), 120, 4000);
        assert!(out.ok, "{}", out.summary);
        assert_eq!(out.json.unwrap()["passed"], 4);
        let broken = interpret(&["--all"], &raw(0, "{\n  \"ok\": true,\n"), 120, 4000);
        assert!(!broken.ok);
    }

    #[test]
    fn long_summaries_are_clipped() {
        let big = format!(
            "{{\"ok\":true,\"passed\":1,\"failed\":[],\"note\":\"{}\"}}",
            "x".repeat(5000)
        );
        let out = interpret(&["--all"], &raw(0, &big), 120, 100);
        assert!(out.ok);
        assert!(out.summary.contains("[... clipped"));
    }
}
