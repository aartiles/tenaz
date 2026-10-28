//! The repairs on a streamed chat completion: a watched call's argument
//! deltas are held back and sent whole, repaired, before its choice ends.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::{ObjectArgs, Repair};
use crate::sse::SseFramer;

/// A tool call whose arguments are being held back.
#[derive(Debug)]
struct Held {
    tool: String,
    arguments: String,
}

/// Rewrites an SSE chat completion stream. Argument deltas of a tool call
/// with watched fields are held back (sent with empty `arguments`) and the
/// whole arguments, repaired when needed, go out in one delta just before
/// the chunk that finishes the choice, or before `[DONE]`. Events that need
/// no change are passed through as received.
#[derive(Debug, Default)]
pub struct StreamRepair {
    watched: ObjectArgs,
    framer: SseFramer,
    /// Held calls by (choice index, tool call index).
    held: BTreeMap<(u64, u64), Held>,
    /// The last chunk's `id`, `model` and `created`, for the deltas it adds.
    envelope: Map<String, Value>,
    pub repairs: Vec<Repair>,
}

impl StreamRepair {
    pub fn new(watched: ObjectArgs) -> Self {
        Self {
            watched,
            ..Self::default()
        }
    }

    /// The bytes to send for `chunk`: complete events, possibly rewritten.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = String::new();
        for event in self.framer.push(chunk) {
            self.event(&event, &mut out);
        }
        out.into_bytes()
    }

    /// The bytes still owed at the end of the stream.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = String::new();
        let rest = self.framer.finish();
        if !rest.trim().is_empty() {
            self.event(rest.trim_end_matches('\n'), &mut out);
        }
        self.release(None, &mut out);
        out.into_bytes()
    }

    /// Passes `event` on, with the argument deltas of watched calls held
    /// back, and sends a choice's held arguments before it finishes.
    fn event(&mut self, event: &str, out: &mut String) {
        let data = event
            .lines()
            .find_map(|l| l.strip_prefix("data:"))
            .map(str::trim);
        if data == Some("[DONE]") {
            self.release(None, out);
            push_event(out, event);
            return;
        }
        let Some(mut json) = data.and_then(|d| serde_json::from_str::<Value>(d).ok()) else {
            push_event(out, event);
            return;
        };
        self.remember_envelope(&json);
        let mut changed = false;
        let mut finishing = Vec::new();
        for choice in json
            .get_mut("choices")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            let index = choice.get("index").and_then(Value::as_u64).unwrap_or(0);
            changed |= self.hold_arguments(index, choice);
            if choice.get("finish_reason").is_some_and(|r| !r.is_null()) {
                finishing.push(index);
            }
        }
        for index in finishing {
            self.release(Some(index), out);
        }
        if changed {
            push_event(out, &format!("data: {json}"));
        } else {
            push_event(out, event);
        }
    }

    /// Keeps the chunk's `id`, `model`, `created` and `object` for the
    /// deltas the repair adds.
    fn remember_envelope(&mut self, json: &Value) {
        for key in ["id", "model", "created", "object"] {
            if let Some(v) = json.get(key) {
                self.envelope.insert(key.to_owned(), v.clone());
            }
        }
    }

    /// Holds back the argument deltas of the watched calls in `choice`;
    /// whether any was taken out.
    fn hold_arguments(&mut self, index: u64, choice: &mut Value) -> bool {
        let mut changed = false;
        for call in choice
            .pointer_mut("/delta/tool_calls")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            let key = (
                index,
                call.get("index").and_then(Value::as_u64).unwrap_or(0),
            );
            self.watch_call(key, call);
            changed |= self.hold(key, call);
        }
        changed
    }

    /// Starts holding the call at `key` when it names a watched tool.
    fn watch_call(&mut self, key: (u64, u64), call: &Value) {
        let Some(name) = call.pointer("/function/name").and_then(Value::as_str) else {
            return;
        };
        if self.watched.fields(name).is_some() {
            self.held.entry(key).or_insert_with(|| Held {
                tool: name.to_owned(),
                arguments: String::new(),
            });
        }
    }

    /// Moves the call's argument delta into what is held for `key`;
    /// whether there was one to move.
    fn hold(&mut self, key: (u64, u64), call: &mut Value) -> bool {
        let Some(held) = self.held.get_mut(&key) else {
            return false;
        };
        let Some(Value::String(part)) = call.pointer_mut("/function/arguments") else {
            return false;
        };
        if part.is_empty() {
            return false;
        }
        held.arguments.push_str(part);
        part.clear();
        true
    }

    /// Sends the held arguments of `choice` (all choices with `None`).
    fn release(&mut self, choice: Option<u64>, out: &mut String) {
        let keys: Vec<(u64, u64)> = self
            .held
            .keys()
            .filter(|(c, _)| choice.is_none_or(|wanted| *c == wanted))
            .copied()
            .collect();
        for key in keys {
            let Some(held) = self.held.remove(&key) else {
                continue;
            };
            let arguments = match self.watched.repair(&held.tool, &held.arguments) {
                Some((fixed, fields)) => {
                    self.repairs.push(Repair {
                        tool: held.tool.clone(),
                        fields,
                    });
                    fixed
                }
                None => held.arguments,
            };
            let mut chunk = self.envelope.clone();
            chunk.insert(
                "choices".to_owned(),
                serde_json::json!([{
                    "index": key.0,
                    "delta": { "tool_calls": [{ "index": key.1, "function": { "arguments": arguments } }] },
                    "finish_reason": null
                }]),
            );
            push_event(out, &format!("data: {}", Value::Object(chunk)));
        }
    }
}

fn push_event(out: &mut String, event: &str) {
    out.push_str(event);
    out.push_str("\n\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repair::fx_tools;

    /// What `stream`, sent in one chunk, comes out as, and the repairs made.
    fn streamed(watched: ObjectArgs, stream: &str) -> (String, Vec<Repair>) {
        let mut repair = StreamRepair::new(watched);
        let mut out = repair.push(stream.as_bytes());
        out.extend(repair.finish());
        (String::from_utf8(out).unwrap(), repair.repairs)
    }

    /// The arguments a client would assemble from a stream.
    fn assembled(stream: &str) -> (String, Option<String>) {
        let mut args = String::new();
        let mut finish = None;
        for event in stream.split("\n\n") {
            let Some(data) = event.strip_prefix("data: ") else {
                continue;
            };
            if data == "[DONE]" {
                continue;
            }
            let json: Value = serde_json::from_str(data).unwrap();
            if let Some(a) = json
                .pointer("/choices/0/delta/tool_calls/0/function/arguments")
                .and_then(Value::as_str)
            {
                args.push_str(a);
            }
            if let Some(f) = json
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
            {
                finish = Some(f.to_owned());
            }
        }
        (args, finish)
    }

    #[test]
    fn streamed_arguments_are_held_repaired_and_sent_before_the_finish() {
        let watched = ObjectArgs::from_request(&fx_tools());
        let chunks = [
            r#"data: {"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"ok"},"finish_reason":null}]}"#,
            r#"data: {"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"t","type":"function","function":{"name":"shell","arguments":"{\"request\":\""}}]},"finish_reason":null}]}"#,
            r#"data: {"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\\\"action\\\": \\\"run\\\", \\\"command\\\": \\\"ls\\\"}"}}]},"finish_reason":null}]}"#,
            r#"data: {"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"}"}}]},"finish_reason":null}]}"#,
            r#"data: {"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "data: [DONE]",
        ];
        let mut repair = StreamRepair::new(watched);
        let mut out = Vec::new();
        // Split mid-event to check the buffering.
        let whole = chunks.join("\n\n") + "\n\n";
        let (a, b) = whole.split_at(150);
        out.extend(repair.push(a.as_bytes()));
        out.extend(repair.push(b.as_bytes()));
        out.extend(repair.finish());
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.starts_with(chunks[0]),
            "untouched events pass as received"
        );
        let (args, finish) = assembled(&out);
        let args: Value = serde_json::from_str(&args).unwrap();
        assert_eq!(args["request"]["command"], "ls");
        assert_eq!(finish.as_deref(), Some("tool_calls"));
        assert!(
            out.find("\"arguments\":\"{\\\"request\\\":{").unwrap()
                < out.find("\"finish_reason\":\"tool_calls\"").unwrap()
        );
        assert_eq!(repair.repairs.len(), 1);
        assert!(out.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn a_character_split_across_chunks_reaches_the_harness_whole() {
        let mut repair = StreamRepair::new(ObjectArgs::from_request(&fx_tools()));
        let event = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"año\"}}]}\n\n";
        // Cut between the two UTF-8 bytes of `ñ`.
        let at = event.find('ñ').unwrap() + 1;
        let mut out = repair.push(&event.as_bytes()[..at]);
        out.extend(repair.push(&event.as_bytes()[at..]));
        out.extend(repair.finish());
        assert_eq!(String::from_utf8(out).unwrap(), event);
    }

    #[test]
    fn unwatched_tools_stream_through_unchanged() {
        let watched = ObjectArgs::from_request(&fx_tools());
        let stream = concat!(
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"t","function":{"name":"read_file","arguments":"{\"path\":"}}]},"finish_reason":null}]}"#,
            "\n\n",
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            "\n\n",
            "data: [DONE]\n\n"
        );
        let (out, repairs) = streamed(watched, stream);
        assert_eq!(out, stream);
        assert!(repairs.is_empty());
    }

    #[test]
    fn arguments_that_need_no_repair_are_sent_whole() {
        let watched = ObjectArgs::from_request(&fx_tools());
        let stream = concat!(
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"t","function":{"name":"shell","arguments":"{\"request\":{\"action\":\"run\","}}]},"finish_reason":null}]}"#,
            "\n\n",
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"command\":\"ls\"}}"}}]},"finish_reason":null}]}"#,
            "\n\n",
        );
        let (out, repairs) = streamed(watched, stream);
        let (args, _) = assembled(&out);
        assert_eq!(args, r#"{"request":{"action":"run","command":"ls"}}"#);
        assert!(repairs.is_empty());
    }
}
