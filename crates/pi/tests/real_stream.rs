//! A stream captured from Pi 1.1.0 (`pi --mode rpc --no-session --model
//! sushi/... --thinking low --tools read,bash,write,edit,grep,find,ls
//! --no-mcp --no-extensions --no-skills --approve --offline`) answering one
//! prompt through Sushi, replayed through the translator.

use tenaz_core::session::{SECOND_COMPACTION, SessionEvent, StopReason, Usage};
use tenaz_pi::Translator;

#[test]
fn a_captured_pi_1_1_0_prompt_translates_to_one_turn() {
    let raw = include_str!("fixtures/pi-1.1.0-pong.rpc.jsonl");
    let mut translator = Translator::new();
    let events: Vec<SessionEvent> = raw
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .flat_map(|record| translator.event(&record))
        .collect();
    let thinking = "The user wants me to reply with the single word \"pong\".";
    assert_eq!(
        events,
        vec![
            SessionEvent::TurnStart,
            SessionEvent::FirstToken,
            SessionEvent::MessageEnd {
                server: None,
                sent: None,
                usage: Usage {
                    input: 2316,
                    output: 17,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 13,
                },
                stop_reason: StopReason::Stop,
                error_message: None,
                thinking_chars: thinking.chars().count(),
                text: "pong".into(),
            },
            SessionEvent::TurnEnd,
            SessionEvent::Settled { aborted: false },
        ]
    );
}

/// Pi 1.4.0 in the Digger 3D run of 2026-10-27: the watchdog aborts at the
/// second compaction, and Pi reports the request it cancels as an error
/// ("This operation was aborted"). That is the abort, not an engine failure
/// to rerun without counting the attempt.
#[test]
fn a_request_pi_cancels_for_an_abort_is_not_an_engine_failure() {
    use tenaz_core::params::RunParams;
    use tenaz_core::role::Role;
    use tenaz_core::watchdog::{Action, WatchConfig, Watchdog};

    let raw = include_str!("fixtures/pi-1.4.0-abort-at-compaction.rpc.jsonl");
    let mut translator = Translator::new();
    let mut watchdog = Watchdog::new(
        RunParams::default(),
        Some(WatchConfig {
            turns: 40,
            context_tokens: 42_598,
            idle: true,
            wrap_up: Role::Bootstrap.wrap_up(),
        }),
    );
    let mut actions = Vec::new();
    for line in raw.lines().filter(|l| !l.is_empty()) {
        let record: serde_json::Value = serde_json::from_str(line).unwrap();
        for event in translator.event(&record) {
            actions.extend(watchdog.handle(&event, 0, String::new));
        }
    }
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::Abort { reason } if reason == SECOND_COMPACTION)),
        "{actions:?}"
    );
    let metrics = watchdog.into_metrics();
    assert_eq!(metrics.provider_errors, 0);
    assert_eq!(metrics.last_stop_reason, Some(StopReason::Aborted));
    assert!(metrics.aborted);
    assert!(!metrics.engine_failed(), "{metrics:?}");
    assert!(
        !metrics.events.iter().any(|e| e.kind == "provider-error"),
        "{:?}",
        metrics.events
    );
}
