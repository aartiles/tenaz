//! The Pi harness against the scripted `fake-pi` binary: the command line,
//! a tool turn with its raw log, several prompts in one session, a loop
//! that is aborted, a steer that Pi delivers, an engine error, a refused
//! prompt, the stderr log, and a close that has to kill.

use std::path::Path;
use std::time::{Duration, Instant};

use tenaz_core::ports::{Harness, HarnessError, Session, SessionSpec};
use tenaz_core::session::{SessionEvent, StopReason, capacity_limit};
use tenaz_pi::{PI_TOOLS, PiHarness, PiLaunch};

fn fake_pi() -> &'static str {
    env!("CARGO_BIN_EXE_fake-pi")
}

fn harness(dir: &Path) -> PiHarness {
    PiLaunch::new(fake_pi(), "fake/fake")
        .thinking("low")
        .skills(vec![dir.join("skills/rust")])
        .env(vec![(
            "FAKE_PI_ARGS".to_owned(),
            dir.join("args.txt").display().to_string(),
        )])
        .harness("fake-pi 0.0.1".to_owned())
}

fn spec(dir: &Path, persist: bool) -> SessionSpec {
    SessionSpec {
        label: "001-builder-T001".into(),
        cwd: dir.to_path_buf(),
        persist_dir: persist.then(|| dir.join("private/sessions")),
        raw_log: persist.then(|| dir.join("private/001-builder-T001.rpc.jsonl")),
        thinking_log: None,
        stderr_log: persist.then(|| dir.join("private/001-builder-T001.stderr.log")),
    }
}

/// Events until `Settled`, with `on_event` run on each one first.
async fn drain(
    session: &mut Box<dyn Session>,
    mut on_event: impl AsyncFnMut(&mut Box<dyn Session>, &SessionEvent),
) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), session.next_event())
            .await
            .expect("an event within 10 s")
            .expect("the stream stays open until settled");
        on_event(session, &event).await;
        let settled = matches!(event, SessionEvent::Settled { .. });
        events.push(event);
        if settled {
            return events;
        }
    }
}

fn kinds(events: &[SessionEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| {
            serde_json::to_value(e).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

#[tokio::test]
async fn a_tool_turn_round_trips_with_the_task_loops_command_line() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("private")).unwrap();
    let harness = harness(dir.path());
    assert_eq!(harness.info().name, "pi");
    let mut session = harness.open(spec(dir.path(), true)).await.unwrap();
    session.prompt("tool").await.unwrap();
    let events = drain(&mut session, async |_, _| {}).await;
    assert_eq!(
        kinds(&events),
        [
            "turn_start",
            "first_token",
            "tool_call_start",
            "tool_call_end",
            "message_end",
            "turn_end",
            "turn_start",
            "first_token",
            "message_end",
            "turn_end",
            "settled"
        ]
    );
    assert!(
        matches!(&events[2], SessionEvent::ToolCallStart { id, name, args } if id == "call_1" && name == "bash" && args["command"] == "ls")
    );
    assert!(
        matches!(&events[3], SessionEvent::ToolCallEnd { result_text, is_error: false, .. } if result_text == "a.txt\u{2028}b.txt")
    );
    assert!(
        matches!(&events[4], SessionEvent::MessageEnd { usage, stop_reason: StopReason::ToolUse, thinking_chars: 4, .. } if usage.input == 1010 && usage.output == 20)
    );
    assert!(
        matches!(&events[8], SessionEvent::MessageEnd { stop_reason: StopReason::Stop, text, usage, .. } if text == "Done." && usage.cache_read == 0)
    );

    // A second prompt in the same session (the preflight sends several) carries its own id.
    session.prompt("hello").await.unwrap();
    let again = drain(&mut session, async |_, _| {}).await;
    assert!(
        matches!(&again[2], SessionEvent::MessageEnd { text, usage, .. } if text == "echo: hello" && usage.cache_read == 900)
    );

    let exit = session.close(Duration::from_secs(5)).await;
    assert_eq!(exit.code, Some(0));

    let args = std::fs::read_to_string(dir.path().join("args.txt")).unwrap();
    let expected = [
        "--mode",
        "rpc",
        "--session-dir",
        &dir.path().join("private/sessions").display().to_string(),
        "--model",
        "fake/fake",
        "--thinking",
        "low",
        "--tools",
        PI_TOOLS,
        "--no-mcp",
        "--no-extensions",
        "--no-skills",
        "--skill",
        &dir.path().join("skills/rust").display().to_string(),
        "--approve",
        "--offline",
    ]
    .join("\n");
    assert_eq!(args, expected);
    assert!(dir.path().join("private/sessions").is_dir());

    let raw =
        std::fs::read_to_string(dir.path().join("private/001-builder-T001.rpc.jsonl")).unwrap();
    let lines: Vec<serde_json::Value> = raw
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["type"], "response");
    assert_eq!(lines[0]["id"], "prompt-0");
    assert!(lines.iter().any(|l| l["type"] == "tool_execution_end"));
    assert!(
        lines
            .iter()
            .any(|l| l["type"] == "response" && l["id"] == "prompt-1")
    );
    assert_eq!(lines.last().unwrap()["type"], "agent_settled");
}

#[tokio::test]
async fn an_ephemeral_session_passes_no_session() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = harness(dir.path())
        .open(spec(dir.path(), false))
        .await
        .unwrap();
    session.prompt("hi").await.unwrap();
    drain(&mut session, async |_, _| {}).await;
    session.close(Duration::from_secs(5)).await;
    let args = std::fs::read_to_string(dir.path().join("args.txt")).unwrap();
    assert!(args.contains("--no-session\n"), "{args}");
    assert!(!args.contains("--session-dir"), "{args}");
    assert!(!dir.path().join("private").exists());
}

#[tokio::test]
async fn a_loop_is_aborted_with_clear_queue_and_abort() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = harness(dir.path())
        .open(spec(dir.path(), false))
        .await
        .unwrap();
    session.prompt("loop").await.unwrap();
    let mut starts = 0;
    let events = drain(&mut session, async |session, event| {
        if matches!(event, SessionEvent::ToolCallStart { .. }) {
            starts += 1;
            if starts == 5 {
                session.abort().await.unwrap();
            }
        }
    })
    .await;
    assert!(starts >= 5);
    let last_message = events
        .iter()
        .rev()
        .find(|e| matches!(e, SessionEvent::MessageEnd { .. }))
        .unwrap();
    assert!(
        matches!(last_message, SessionEvent::MessageEnd { stop_reason: StopReason::Aborted, text, .. } if text == "stopped")
    );
    assert_eq!(
        events.last(),
        Some(&SessionEvent::Settled { aborted: true })
    );
    assert_eq!(session.close(Duration::from_secs(5)).await.code, Some(0));
}

#[tokio::test]
async fn a_steer_reaches_the_agent_between_turns() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = harness(dir.path())
        .open(spec(dir.path(), false))
        .await
        .unwrap();
    session.prompt("steer").await.unwrap();
    let mut steered = false;
    let events = drain(&mut session, async |session, event| {
        if matches!(event, SessionEvent::ToolCallEnd { .. }) && !steered {
            steered = true;
            session.steer("[orchestrator] stop reading").await.unwrap();
        }
    })
    .await;
    let last_message = events
        .iter()
        .rev()
        .find(|e| matches!(e, SessionEvent::MessageEnd { .. }))
        .unwrap();
    assert!(
        matches!(last_message, SessionEvent::MessageEnd { stop_reason: StopReason::Stop, text, .. } if text == "steered: [orchestrator] stop reading")
    );
    assert_eq!(
        events.last(),
        Some(&SessionEvent::Settled { aborted: false })
    );
    session.close(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn an_engine_error_keeps_its_capacity_message() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = harness(dir.path())
        .open(spec(dir.path(), false))
        .await
        .unwrap();
    session.prompt("error").await.unwrap();
    let events = drain(&mut session, async |_, _| {}).await;
    let errors: Vec<&SessionEvent> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                SessionEvent::MessageEnd {
                    stop_reason: StopReason::Error,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(errors.len(), 1);
    let SessionEvent::MessageEnd {
        error_message: Some(message),
        ..
    } = errors[0]
    else {
        panic!("{:?}", errors[0]);
    };
    assert_eq!(capacity_limit(message), Some(61488));
    assert_eq!(
        events.last(),
        Some(&SessionEvent::Settled { aborted: false })
    );
    session.close(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn a_refused_prompt_ends_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = harness(dir.path())
        .open(spec(dir.path(), false))
        .await
        .unwrap();
    session.prompt("reject").await.unwrap();
    let events = drain(&mut session, async |_, _| {}).await;
    assert_eq!(
        events,
        vec![
            SessionEvent::PromptRejected {
                reason: "Model not found: fake/fake".into()
            },
            SessionEvent::Settled { aborted: true }
        ]
    );
    session.prompt("handled").await.unwrap();
    let handled = drain(&mut session, async |_, _| {}).await;
    assert!(matches!(&handled[0], SessionEvent::PromptRejected { reason } if reason == "handled"));
    session.close(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn stderr_goes_to_the_stderr_log() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("private")).unwrap();
    let mut session = harness(dir.path())
        .open(spec(dir.path(), true))
        .await
        .unwrap();
    session.prompt("stderr").await.unwrap();
    drain(&mut session, async |_, _| {}).await;
    session.close(Duration::from_secs(5)).await;
    let log =
        std::fs::read_to_string(dir.path().join("private/001-builder-T001.stderr.log")).unwrap();
    assert!(log.contains("fake stderr: diagnostics go here"), "{log}");
}

#[tokio::test]
async fn a_process_that_ignores_stdin_eof_is_killed_after_the_grace() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = harness(dir.path())
        .open(spec(dir.path(), false))
        .await
        .unwrap();
    session.prompt("linger").await.unwrap();
    drain(&mut session, async |_, _| {}).await;
    let started = Instant::now();
    let exit = session.close(Duration::from_millis(200)).await;
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(exit.code, None, "killed by a signal, no exit code");
    // The stream has ended; later commands report the closed session.
    assert!(session.next_event().await.is_none());
    assert!(matches!(
        session.prompt("x").await,
        Err(HarnessError::Closed)
    ));
}

#[tokio::test]
async fn a_missing_binary_is_a_spawn_error() {
    let dir = tempfile::tempdir().unwrap();
    let harness = PiLaunch::new("/nonexistent/pi", "fake/fake").harness("0".into());
    let error = harness.open(spec(dir.path(), false)).await.err().unwrap();
    assert!(matches!(error, HarnessError::Spawn(_)), "{error}");
}
