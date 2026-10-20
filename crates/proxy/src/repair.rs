//! Tool-call arguments sent back as text where the tool's schema wants an
//! object. Some engines' tool-call parsers coerce an argument by its schema's
//! `type` and, when the schema has none (a `oneOf` of objects, as fx declares
//! `shell.request`), hand the object back as a JSON string. The harness then
//! rejects every call. The repair is narrow: an argument is parsed back into
//! an object only when its schema cannot accept a string and the string is a
//! JSON object; everything else passes through untouched.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

/// Per tool name, the top-level arguments whose schema accepts only objects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObjectArgs(BTreeMap<String, BTreeSet<String>>);

impl ObjectArgs {
    /// Reads the `tools` of a chat completion request.
    pub fn from_request(request: &Value) -> Self {
        let mut map = BTreeMap::new();
        for tool in request
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(function) = tool.get("function") else {
                continue;
            };
            let Some(name) = function.get("name").and_then(Value::as_str) else {
                continue;
            };
            let fields: BTreeSet<String> = function
                .get("parameters")
                .and_then(|p| p.get("properties"))
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter(|(_, schema)| only_objects(schema))
                .map(|(field, _)| field.clone())
                .collect();
            if !fields.is_empty() {
                map.insert(name.to_owned(), fields);
            }
        }
        Self(map)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The fields to watch for `tool`, if any.
    pub fn fields(&self, tool: &str) -> Option<&BTreeSet<String>> {
        self.0.get(tool)
    }
}

/// A schema that accepts objects and nothing else: `type: object`, or a
/// `oneOf`/`anyOf` whose every variant does.
fn only_objects(schema: &Value) -> bool {
    match schema.get("type") {
        Some(Value::String(t)) => return t == "object",
        Some(_) => return false,
        None => {}
    }
    ["oneOf", "anyOf"].iter().any(|key| {
        schema
            .get(key)
            .and_then(Value::as_array)
            .is_some_and(|variants| !variants.is_empty() && variants.iter().all(only_objects))
    })
}

/// The arguments with each watched field that holds a JSON object as text
/// parsed back into that object, and the fields repaired; `None` when
/// nothing needed repair (or the arguments are not a JSON object).
pub fn repair_arguments(
    arguments: &str,
    fields: &BTreeSet<String>,
) -> Option<(String, Vec<String>)> {
    let mut object: Map<String, Value> = serde_json::from_str(arguments).ok()?;
    let mut repaired = Vec::new();
    for field in fields {
        let Some(Value::String(text)) = object.get(field) else {
            continue;
        };
        if let Ok(inner @ Value::Object(_)) = serde_json::from_str::<Value>(text) {
            object.insert(field.clone(), inner);
            repaired.push(field.clone());
        }
    }
    if repaired.is_empty() {
        return None;
    }
    Some((serde_json::to_string(&object).ok()?, repaired))
}

/// One repaired tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repair {
    pub tool: String,
    pub fields: Vec<String>,
}

/// Repairs the tool calls of a non-streaming chat completion body.
pub fn repair_body(body: &[u8], watched: &ObjectArgs) -> Option<(Vec<u8>, Vec<Repair>)> {
    let mut json: Value = serde_json::from_slice(body).ok()?;
    let mut repairs = Vec::new();
    for choice in json
        .get_mut("choices")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        for call in choice
            .pointer_mut("/message/tool_calls")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            let Some(function) = call.get_mut("function") else {
                continue;
            };
            let Some(name) = function
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
            else {
                continue;
            };
            let Some(fields) = watched.fields(&name) else {
                continue;
            };
            let Some(arguments) = function.get("arguments").and_then(Value::as_str) else {
                continue;
            };
            if let Some((fixed, repaired)) = repair_arguments(arguments, fields) {
                function["arguments"] = Value::String(fixed);
                repairs.push(Repair {
                    tool: name,
                    fields: repaired,
                });
            }
        }
    }
    if repairs.is_empty() {
        return None;
    }
    Some((serde_json::to_vec(&json).ok()?, repairs))
}

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
    /// Pending text not yet terminated by a blank line.
    buffer: String,
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
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        let mut out = String::new();
        while let Some(at) = self.buffer.find("\n\n") {
            let event = self.buffer[..at].to_owned();
            self.buffer.drain(..at + 2);
            self.event(&event, &mut out);
        }
        out.into_bytes()
    }

    /// The bytes still owed at the end of the stream.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = String::new();
        let rest = std::mem::take(&mut self.buffer);
        if !rest.trim().is_empty() {
            self.event(rest.trim_end_matches('\n'), &mut out);
        }
        self.release(None, &mut out);
        out.into_bytes()
    }

    fn event(&mut self, event: &str, out: &mut String) {
        let data = event
            .lines()
            .find_map(|l| l.strip_prefix("data:"))
            .map(str::trim);
        let Some(data) = data else {
            push_event(out, event);
            return;
        };
        if data == "[DONE]" {
            self.release(None, out);
            push_event(out, event);
            return;
        }
        let Ok(mut json) = serde_json::from_str::<Value>(data) else {
            push_event(out, event);
            return;
        };
        for key in ["id", "model", "created", "object"] {
            if let Some(v) = json.get(key) {
                self.envelope.insert(key.to_owned(), v.clone());
            }
        }
        let mut changed = false;
        let mut finishing = Vec::new();
        for choice in json
            .get_mut("choices")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            let index = choice.get("index").and_then(Value::as_u64).unwrap_or(0);
            for call in choice
                .pointer_mut("/delta/tool_calls")
                .and_then(Value::as_array_mut)
                .into_iter()
                .flatten()
            {
                let call_index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                let key = (index, call_index);
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str)
                    && !self.held.contains_key(&key)
                    && self.watched.fields(name).is_some()
                {
                    self.held.insert(
                        key,
                        Held {
                            tool: name.to_owned(),
                            arguments: String::new(),
                        },
                    );
                }
                let Some(held) = self.held.get_mut(&key) else {
                    continue;
                };
                if let Some(Value::String(part)) = call.pointer_mut("/function/arguments")
                    && !part.is_empty()
                {
                    held.arguments.push_str(part);
                    part.clear();
                    changed = true;
                }
            }
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
            let arguments = match self
                .watched
                .fields(&held.tool)
                .and_then(|fields| repair_arguments(&held.arguments, fields))
            {
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

    /// fx 0.0.14's `shell` tool, cut down: `request` is a `oneOf` of objects.
    fn fx_tools() -> Value {
        let run = serde_json::json!({"type": "object", "properties": {"action": {"type": "string", "enum": ["run"]}, "command": {"type": "string"}}, "required": ["action", "command"]});
        let poll = serde_json::json!({"type": "object", "properties": {"action": {"type": "string", "enum": ["poll"]}}});
        serde_json::json!({"tools": [
            {"type": "function", "function": {"name": "shell", "parameters": {"type": "object", "properties": {"request": {"oneOf": [run, poll]}}}}},
            {"type": "function", "function": {"name": "read_file", "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "options": {"anyOf": [{"type": "object"}, {"type": "string"}]}}}}}
        ]})
    }

    #[test]
    fn only_arguments_that_cannot_be_strings_are_watched() {
        let watched = ObjectArgs::from_request(&fx_tools());
        assert_eq!(
            watched.fields("shell"),
            Some(&BTreeSet::from(["request".to_owned()]))
        );
        assert_eq!(
            watched.fields("read_file"),
            None,
            "options accepts a string"
        );
        assert!(ObjectArgs::from_request(&serde_json::json!({"messages": []})).is_empty());
    }

    #[test]
    fn a_stringified_object_is_parsed_back_and_nothing_else_is_touched() {
        let fields = BTreeSet::from(["request".to_owned()]);
        let (fixed, repaired) = repair_arguments(
            r#"{"request":"{\"action\": \"run\", \"command\": \"ls\"}"}"#,
            &fields,
        )
        .unwrap();
        assert_eq!(repaired, ["request"]);
        let fixed: Value = serde_json::from_str(&fixed).unwrap();
        assert_eq!(fixed["request"]["command"], "ls");
        // Already an object, a string that is not a JSON object, not JSON at all.
        assert!(repair_arguments(r#"{"request":{"action":"run"}}"#, &fields).is_none());
        assert!(repair_arguments(r#"{"request":"ls -la"}"#, &fields).is_none());
        assert!(repair_arguments(r#"{"request":"[1, 2]"}"#, &fields).is_none());
        assert!(repair_arguments("not json", &fields).is_none());
    }

    #[test]
    fn non_streaming_tool_calls_are_repaired() {
        let watched = ObjectArgs::from_request(&fx_tools());
        let body = serde_json::json!({"choices": [{"index": 0, "message": {"tool_calls": [
            {"id": "a", "type": "function", "function": {"name": "shell", "arguments": "{\"request\":\"{\\\"action\\\":\\\"run\\\",\\\"command\\\":\\\"ls\\\"}\"}"}},
            {"id": "b", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"x\",\"options\":\"{}\"}"}}
        ]}, "finish_reason": "tool_calls"}]});
        let (fixed, repairs) = repair_body(body.to_string().as_bytes(), &watched).unwrap();
        assert_eq!(
            repairs,
            [Repair {
                tool: "shell".into(),
                fields: vec!["request".into()]
            }]
        );
        let fixed: Value = serde_json::from_slice(&fixed).unwrap();
        let shell: Value = serde_json::from_str(
            fixed["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(shell["request"]["action"], "run");
        assert_eq!(
            fixed["choices"][0]["message"]["tool_calls"][1]["function"]["arguments"],
            "{\"path\":\"x\",\"options\":\"{}\"}",
            "a string where strings are allowed stays"
        );
        assert!(repair_body(br#"{"choices":[{"message":{"content":"hi"}}]}"#, &watched).is_none());
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
    fn unwatched_tools_stream_through_unchanged() {
        let watched = ObjectArgs::from_request(&fx_tools());
        let stream = concat!(
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"t","function":{"name":"read_file","arguments":"{\"path\":"}}]},"finish_reason":null}]}"#,
            "\n\n",
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            "\n\n",
            "data: [DONE]\n\n"
        );
        let mut repair = StreamRepair::new(watched);
        let mut out = repair.push(stream.as_bytes());
        out.extend(repair.finish());
        assert_eq!(String::from_utf8(out).unwrap(), stream);
        assert!(repair.repairs.is_empty());
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
        let mut repair = StreamRepair::new(watched);
        let mut out = repair.push(stream.as_bytes());
        out.extend(repair.finish());
        let (args, _) = assembled(&String::from_utf8(out).unwrap());
        assert_eq!(args, r#"{"request":{"action":"run","command":"ls"}}"#);
        assert!(repair.repairs.is_empty());
    }
}
