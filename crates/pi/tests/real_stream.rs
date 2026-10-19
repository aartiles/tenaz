//! A stream captured from Pi 1.1.0 (`pi --mode rpc --no-session --model
//! sushi/... --thinking low --tools read,bash,write,edit,grep,find,ls
//! --no-mcp --no-extensions --no-skills --approve --offline`) answering one
//! prompt through Sushi, replayed through the translator.

use tenaz_core::session::{SessionEvent, StopReason, Usage};
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
