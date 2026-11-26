//! What a continued session is told the sessions before it did (ADR 0045).

use super::tests::{context_stop, engine_error, params, steps, turns_stop};
use super::*;
use crate::trail::testing::{reply, trail, write};

/// A context stop whose session wrote `path` and said `words`.
fn context_stop_after(path: &str, words: &str) -> SessionMetrics {
    let mut stopped = context_stop();
    let [started, ended] = write("1", path);
    stopped.trail = Box::new(trail(&[started, ended, reply(words, None)]));
    stopped
}

fn engine_error_after_writing(path: &str) -> SessionMetrics {
    let mut failed = engine_error();
    failed.trail = Box::new(trail(&write("1", path)));
    failed
}

/// What the stopped session did follows the note.
#[test]
fn a_continued_session_gets_what_the_stopped_one_did() {
    let (recovery, _) = steps(
        Role::Builder,
        &[context_stop_after("src/a.js", "Fixing a.")],
    );
    let note = recovery.note();
    assert!(
        note.starts_with(&format!(
            "{CONTINUATION}\n\n### What the earlier sessions on this work did"
        )),
        "{note}"
    );
    assert!(note.contains("`src/a.js`"), "{note}");
    assert!(note.ends_with("- The last words: \"Fixing a.\""), "{note}");
}

#[test]
fn a_continued_session_that_left_nothing_to_tell_gets_the_note_alone() {
    let (recovery, _) = steps(Role::Builder, &[context_stop()]);
    assert_eq!(recovery.note(), CONTINUATION);
}

/// Each continuation hears of every session since the work started, with
/// the latest words.
#[test]
fn a_second_continuation_adds_to_what_the_first_one_did() {
    let (recovery, _) = steps(
        Role::Builder,
        &[
            context_stop_after("src/a.js", "A."),
            context_stop_after("src/b.js", "B."),
        ],
    );
    let note = recovery.note();
    assert!(note.contains("`src/a.js`, `src/b.js`"), "{note}");
    assert!(note.ends_with("\"B.\""), "{note}");
}

#[test]
fn a_continuation_that_did_nothing_keeps_what_came_before() {
    let (recovery, _) = steps(
        Role::Builder,
        &[context_stop_after("src/a.js", "A."), context_stop()],
    );
    assert!(
        recovery.note().contains("`src/a.js`"),
        "{}",
        recovery.note()
    );
}

/// A turn stop continues with its own note, and the digest after it.
#[test]
fn a_turn_continuation_gets_the_digest_too() {
    let mut stopped = turns_stop();
    stopped.trail = context_stop_after("src/a.js", "A.").trail;
    let (recovery, _) = steps(Role::Builder, &[stopped]);
    let note = recovery.note();
    assert!(note.starts_with(TURN_CONTINUATION), "{note}");
    assert!(note.contains("`src/a.js`"), "{note}");
}

/// The rerun of a continued session the engine failed hears of what that
/// session wrote too.
#[test]
fn an_engine_retry_adds_what_the_failed_session_wrote() {
    let (recovery, taken) = steps(
        Role::Builder,
        &[
            context_stop_after("src/a.js", "A."),
            engine_error_after_writing("src/b.js"),
        ],
    );
    assert!(matches!(taken[1], Step::Retry { .. }), "{taken:?}");
    assert!(
        recovery.note().contains("`src/a.js`, `src/b.js`"),
        "{}",
        recovery.note()
    );
}

/// A refused request continues with its own note, and the digest after it.
#[test]
fn a_refused_request_continuation_gets_the_digest_too() {
    let mut refused = engine_error();
    refused.request_refusal = Some("400: cannot decode".to_owned());
    refused.trail = context_stop_after("src/a.js", "A.").trail;
    let (recovery, taken) = steps(Role::Builder, &[refused]);
    assert!(
        matches!(
            taken[0],
            Step::Continue {
                kind: Continuation::Refused(_),
                ..
            }
        ),
        "{taken:?}"
    );
    let note = recovery.note();
    assert!(
        note.starts_with(&refusal_continuation(
            Refused::Request,
            "400: cannot decode"
        )),
        "{note}"
    );
    assert!(note.contains("`src/a.js`"), "{note}");
}

#[test]
fn an_image_continuation_gets_the_digest_too() {
    let mut refused = engine_error();
    refused.image_refusal = Some("at most 1 images".to_owned());
    refused.trail = context_stop_after("src/a.js", "A.").trail;
    let (recovery, _) = steps(Role::Builder, &[refused]);
    let note = recovery.note();
    assert!(
        note.starts_with(&refusal_continuation(Refused::Image, "at most 1 images")),
        "{note}"
    );
    assert!(note.contains("`src/a.js`"), "{note}");
}

/// A session resumed after a pause knows nothing of the sessions before
/// the pause; a continuation of it hears of the sessions since.
#[test]
fn after_a_pause_the_digest_covers_the_sessions_since() {
    let mut recovery = Recovery::resuming(true);
    assert_eq!(recovery.note(), CONTINUATION);
    let step = recovery.next(
        &context_stop_after("src/b.js", "B."),
        Role::Builder,
        &params(),
    );
    assert!(matches!(step, Step::Continue { .. }), "{step:?}");
    let note = recovery.note();
    assert!(note.contains("`src/b.js`."), "{note}");
}
