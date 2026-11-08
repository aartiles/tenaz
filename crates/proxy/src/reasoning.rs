//! Removing the reasoning of earlier assistant messages (ADR 0023), in
//! steps the engine's prefix cache can live with (ADR 0028).
//!
//! A harness sends the whole conversation with every request, and many send
//! each earlier assistant message's reasoning back with it. Removing it keeps
//! the window for the work, but every message whose reasoning goes changes
//! the prompt from there on, and the engine reads it all again. So the
//! reasoning stays until what is kept before the newest message passes a
//! budget, and then all of it goes at once: between two such steps each
//! request extends the one before it, and the engine reads only what is new.

use tenaz_core::util::approx_tokens;

use crate::JsonObject;

/// The fields in which harnesses send an assistant message's reasoning.
const REASONING_FIELDS: [&str; 2] = ["reasoning_content", "reasoning"];

/// Removes the reasoning of the earlier assistant messages that a budget of
/// `budget_tokens` leaves out (see [`watermark`]); true when any was
/// removed. The newest assistant message always keeps its own.
pub(crate) fn drop_past_reasoning(request: &mut JsonObject, budget_tokens: u64) -> bool {
    let Some(messages) = request.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return false;
    };
    let mut assistants = messages
        .iter_mut()
        .filter_map(|m| m.as_object_mut())
        .filter(|m| m.get("role").and_then(serde_json::Value::as_str) == Some("assistant"))
        .collect::<Vec<_>>();
    let sizes = assistants
        .iter()
        .map(|m| reasoning_tokens(m))
        .collect::<Vec<_>>();
    let budget = usize::try_from(budget_tokens).unwrap_or(usize::MAX);
    let mut dropped = false;
    for message in &mut assistants[..watermark(&sizes, budget)] {
        for field in REASONING_FIELDS {
            dropped |= message.remove(field).is_some();
        }
    }
    dropped
}

/// The rough tokens of reasoning an assistant message carries. Reasoning
/// that is not text counts as its JSON, so it still takes the budget.
fn reasoning_tokens(message: &JsonObject) -> usize {
    REASONING_FIELDS
        .iter()
        .filter_map(|field| message.get(*field))
        .map(|value| match value {
            serde_json::Value::String(text) => approx_tokens(text),
            other => approx_tokens(&other.to_string()),
        })
        .sum()
}

/// How many of the first assistant messages lose their reasoning, given each
/// one's reasoning size. It replays the conversation one assistant message at
/// a time: when the reasoning kept before the newest message passes `budget`,
/// all of it goes. While the harness only appends messages, the next request
/// gets the same answer up to its new message, so the mark only moves
/// forward, and only in steps. After the harness prunes or compacts its
/// history, the steps start over from what it still sends, and reasoning
/// removed before may be sent again. A budget of 0 moves the mark every
/// turn, as ADR 0023 did.
fn watermark(sizes: &[usize], budget: usize) -> usize {
    let mut mark = 0;
    let mut kept = 0usize;
    for newest in 1..sizes.len() {
        kept = kept.saturating_add(sizes[newest - 1]);
        if kept > budget {
            mark = newest;
            kept = 0;
        }
    }
    mark
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_moves_once_the_kept_reasoning_passes_the_budget() {
        assert_eq!(watermark(&[], 10), 0, "no messages");
        assert_eq!(watermark(&[50], 10), 0, "the newest keeps its own");
        assert_eq!(watermark(&[5, 5, 99], 10), 0, "at the budget it is kept");
        assert_eq!(watermark(&[4, 4, 4, 99], 10), 3, "past it, all goes");
        assert_eq!(
            watermark(&[4, 4, 4, 1, 1, 99], 10),
            3,
            "kept since the step"
        );
        assert_eq!(watermark(&[4, 4, 4, 4, 4, 4, 1], 10), 6, "a second step");
    }

    #[test]
    fn a_zero_budget_keeps_only_the_newest_reasoning() {
        assert_eq!(watermark(&[1, 1, 1, 1], 0), 3);
        assert_eq!(
            watermark(&[0, 0, 0], 0),
            0,
            "the mark moves only past the budget, not at it"
        );
    }

    /// Every conversation of up to six turns of 0, 1, 5 or 11 tokens: the
    /// mark of a longer one is never behind that of the shorter one it
    /// extends, and the earlier reasoning kept stays within the budget.
    #[test]
    fn the_mark_never_goes_back_and_the_kept_reasoning_fits_the_budget() {
        const SIZES: [usize; 4] = [0, 1, 5, 11];
        for code in 0..SIZES.len().pow(6) {
            let sizes = (0..6)
                .map(|digit| SIZES[code / SIZES.len().pow(digit) % SIZES.len()])
                .collect::<Vec<_>>();
            for budget in [0, 5, 10] {
                for k in 1..sizes.len() {
                    let mark = watermark(&sizes[..k], budget);
                    assert!(
                        mark <= watermark(&sizes[..=k], budget),
                        "{sizes:?} {budget} {k}"
                    );
                    let kept: usize = sizes[mark..k - 1].iter().sum();
                    assert!(kept <= budget, "{sizes:?} {budget} {k}");
                }
            }
        }
    }

    /// Bytes of reasoning in each assistant turn (10 tokens), and the
    /// budget in tokens.
    const TURN_CHARS: usize = 40;
    const BUDGET_TOKENS: u64 = 25;

    /// The requests at which the earlier reasoning passes the budget: it
    /// takes this many turns' worth, and each step starts counting again.
    fn is_step(n: usize) -> bool {
        let turn_tokens = approx_tokens(&"r".repeat(TURN_CHARS));
        let turns_per_step = usize::try_from(BUDGET_TOKENS).unwrap() / turn_tokens + 1;
        n > 1 && (n - 1).is_multiple_of(turns_per_step)
    }

    /// The `n`th request of an agent session: a user message, then `n`
    /// assistant turns that send their reasoning in `field`, each with a
    /// tool result.
    fn session_request(n: usize, field: &str) -> JsonObject {
        let mut messages = vec![serde_json::json!({"role": "user", "content": "go"})];
        for i in 0..n {
            let mut assistant = serde_json::json!({
                "role": "assistant", "content": "", "tool_calls": [{"id": i.to_string()}],
            });
            assistant[field] = "r".repeat(TURN_CHARS).into();
            messages.push(assistant);
            messages.push(
                serde_json::json!({"role": "tool", "tool_call_id": i.to_string(), "content": "out"}),
            );
        }
        serde_json::json!({ "messages": messages })
            .as_object()
            .cloned()
            .unwrap()
    }

    fn rewritten(n: usize, field: &str, budget_tokens: u64) -> Vec<serde_json::Value> {
        let mut request = session_request(n, field);
        drop_past_reasoning(&mut request, budget_tokens);
        request["messages"].as_array().unwrap().clone()
    }

    /// How many leading messages two requests share.
    fn shared(a: &[serde_json::Value], b: &[serde_json::Value]) -> usize {
        a.iter().zip(b).take_while(|(x, y)| x == y).count()
    }

    /// The index of the first assistant message that still has reasoning.
    fn first_with_reasoning(messages: &[serde_json::Value], field: &str) -> usize {
        messages
            .iter()
            .position(|m| m.get(field).is_some())
            .unwrap()
    }

    /// Between steps each request repeats all of the one before it, past
    /// the previous assistant message, so the engine reads only the new
    /// turn; at a step it repeats it up to the oldest reasoning it removes.
    #[test]
    fn consecutive_requests_share_their_prefix() {
        for field in REASONING_FIELDS {
            for n in 2..=12 {
                let before = rewritten(n - 1, field, BUDGET_TOKENS);
                let after = rewritten(n, field, BUDGET_TOKENS);
                let expected = if is_step(n) {
                    first_with_reasoning(&before, field)
                } else {
                    before.len()
                };
                assert_eq!(shared(&before, &after), expected, "{field}, request {n}");
            }
        }
    }

    #[test]
    fn a_zero_budget_cuts_every_request_at_the_previous_assistant_message() {
        for n in 2..=6 {
            let before = rewritten(n - 1, "reasoning_content", 0);
            let after = rewritten(n, "reasoning_content", 0);
            assert_eq!(shared(&before, &after), 2 * (n - 1) - 1, "request {n}");
        }
    }

    #[test]
    fn steps_come_every_third_request_here() {
        let steps = (2..=12).filter(|&n| is_step(n)).collect::<Vec<_>>();
        assert_eq!(steps, [4, 7, 10]);
    }

    #[test]
    fn both_reasoning_fields_count() {
        let both =
            serde_json::json!({"reasoning_content": "r".repeat(40), "reasoning": "r".repeat(40)});
        assert_eq!(reasoning_tokens(both.as_object().unwrap()), 20);
    }

    #[test]
    fn reasoning_that_is_not_text_still_takes_the_budget() {
        let message = serde_json::json!({"reasoning": {"text": "r".repeat(TURN_CHARS)}});
        assert!(reasoning_tokens(message.as_object().unwrap()) > TURN_CHARS / 4);
    }
}
