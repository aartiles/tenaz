//! Tool-call arguments sent back as text where the tool's schema wants an
//! object. Some engines' tool-call parsers coerce an argument by its schema's
//! `type` and, when the schema has none (a `oneOf` of objects, as fx declares
//! `shell.request`), hand the object back as a JSON string. The harness then
//! rejects every call. The repair is narrow: an argument is parsed back into
//! an object only when its schema cannot accept a string and the string is a
//! JSON object; everything else passes through untouched.
//!
//! The other repair is a `command` sent without the `action` that selects
//! the variant: models drop it from fx's `shell` call. It is added only when
//! every variant of the object argument that takes a `command` fixes its
//! `action` to the same value (fx has two `run` variants, one with a custom
//! shell).
//!
//! The last is properties sent beside that argument instead of inside it
//! (`{"request": {...}, "timeout_ms": 5000}`): they are moved in only when
//! the variant its `action` selects takes every one of them and the argument
//! does not already hold them.
//!
//! Finally, an integer outside the range that variant declares for it is
//! clamped to the range: models ask fx's `shell` for a `yield_time_ms` above
//! its 30000 and, once rejected, retry with 30000.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::sse::SseFramer;

/// Per tool name, what the repairs need to know about its arguments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObjectArgs(BTreeMap<String, ToolArgs>);

/// One tool's arguments whose schema accepts only objects, with the
/// properties their variants take per fixed `action`, and the `action` that
/// every variant taking a `command` fixes, if any.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ToolArgs {
    fields: BTreeMap<String, Actions>,
    command_action: Option<String>,
}

/// Per fixed `action`, the properties of the variants that select it, with
/// the integer range each declares.
type Actions = BTreeMap<String, BTreeMap<String, Bounds>>;

/// A property's `minimum` and `maximum`, when they are integers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Bounds {
    minimum: Option<i64>,
    maximum: Option<i64>,
}

impl ObjectArgs {
    /// Reads the `tools` of a chat completion request.
    pub fn from_request(request: &Value) -> Self {
        let mut tools = BTreeMap::new();
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
            let fields: BTreeMap<String, Actions> = function
                .get("parameters")
                .and_then(|p| p.get("properties"))
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter(|(_, schema)| only_objects(schema))
                .map(|(field, schema)| (field.clone(), actions_of(schema)))
                .collect();
            if fields.is_empty() {
                continue;
            }
            let commands: BTreeSet<&String> = fields
                .values()
                .flatten()
                .filter(|(_, properties)| properties.contains_key("command"))
                .map(|(action, _)| action)
                .collect();
            let command_action = commands
                .first()
                .filter(|_| commands.len() == 1)
                .map(|action| (*action).clone());
            tools.insert(
                name.to_owned(),
                ToolArgs {
                    fields,
                    command_action,
                },
            );
        }
        Self(tools)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The fields to watch for `tool`, if any.
    pub fn fields(&self, tool: &str) -> Option<BTreeSet<&str>> {
        self.0
            .get(tool)
            .map(|args| args.fields.keys().map(String::as_str).collect())
    }

    /// The `action` a `command` without one selects for `tool`.
    pub fn command_action(&self, tool: &str) -> Option<&str> {
        self.0.get(tool)?.command_action.as_deref()
    }

    /// Repairs one call of `tool`; `None` when it needs nothing.
    fn repair(&self, tool: &str, arguments: &str) -> Option<(String, Vec<String>)> {
        repair_arguments(arguments, self.0.get(tool)?)
    }
}

/// The variants of `schema` that fix `action` to a single value (`const`,
/// or an `enum` of one), with the properties each value's variants take and
/// their integer ranges (the first variant's, if two disagree).
fn actions_of(schema: &Value) -> Actions {
    let mut actions = Actions::new();
    for variant in ["oneOf", "anyOf"]
        .iter()
        .filter_map(|key| schema.get(key).and_then(Value::as_array))
        .flatten()
    {
        let Some(properties) = variant.get("properties").and_then(Value::as_object) else {
            continue;
        };
        let Some(action) = properties.get("action") else {
            continue;
        };
        let value = match (action.get("const"), action.get("enum")) {
            (Some(Value::String(value)), _) => value,
            (None, Some(Value::Array(values))) => match values.as_slice() {
                [Value::String(value)] => value,
                _ => continue,
            },
            _ => continue,
        };
        let taken = actions.entry(value.clone()).or_default();
        for (name, property) in properties {
            taken.entry(name.clone()).or_insert(Bounds {
                minimum: property.get("minimum").and_then(Value::as_i64),
                maximum: property.get("maximum").and_then(Value::as_i64),
            });
        }
    }
    actions
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
/// parsed back into that object, the tool's `command_action` added to a
/// `command` sent without an `action` (at the top level or in a watched
/// field), properties sent beside a watched field moved into it when its
/// variant takes them all, and integers outside their variant's range
/// clamped to it; the repaired fields, `None` when nothing needed repair (or
/// the arguments are not a JSON object).
fn repair_arguments(arguments: &str, tool: &ToolArgs) -> Option<(String, Vec<String>)> {
    let mut object: Map<String, Value> = serde_json::from_str(arguments).ok()?;
    let mut repaired = Vec::new();
    for field in tool.fields.keys() {
        let Some(Value::String(text)) = object.get(field) else {
            continue;
        };
        if let Ok(inner @ Value::Object(_)) = serde_json::from_str::<Value>(text) {
            object.insert(field.clone(), inner);
            repaired.push(field.clone());
        }
    }
    if let Some(action) = &tool.command_action {
        let missing = |o: &Map<String, Value>| {
            !o.contains_key("action") && o.get("command").is_some_and(Value::is_string)
        };
        if !tool.fields.keys().any(|f| object.contains_key(f)) && missing(&object) {
            object.insert("action".to_owned(), Value::String(action.clone()));
            repaired.push("action".to_owned());
        }
        for field in tool.fields.keys() {
            if let Some(Value::Object(inner)) = object.get_mut(field)
                && missing(inner)
            {
                inner.insert("action".to_owned(), Value::String(action.clone()));
                repaired.push(format!("{field}.action"));
            }
        }
    }
    for (field, actions) in &tool.fields {
        let Some(Value::Object(inner)) = object.get(field) else {
            continue;
        };
        let Some(properties) = inner
            .get("action")
            .and_then(Value::as_str)
            .and_then(|action| actions.get(action))
        else {
            continue;
        };
        let stray: Vec<String> = object
            .keys()
            .filter(|key| !tool.fields.contains_key(*key))
            .cloned()
            .collect();
        if stray.is_empty()
            || !stray
                .iter()
                .all(|key| properties.contains_key(key) && !inner.contains_key(key))
        {
            continue;
        }
        let moved: Vec<(String, Value)> = stray
            .into_iter()
            .filter_map(|key| object.remove_entry(&key))
            .collect();
        if let Some(Value::Object(inner)) = object.get_mut(field) {
            for (key, value) in moved {
                repaired.push(format!("{field}.{key}"));
                inner.insert(key, value);
            }
        }
    }
    if tool.fields.keys().any(|f| object.contains_key(f)) {
        for (field, actions) in &tool.fields {
            if let Some(Value::Object(inner)) = object.get_mut(field) {
                clamp(inner, actions, &format!("{field}."), &mut repaired);
            }
        }
    } else {
        for actions in tool.fields.values() {
            clamp(&mut object, actions, "", &mut repaired);
        }
    }
    if repaired.is_empty() {
        return None;
    }
    Some((serde_json::to_string(&object).ok()?, repaired))
}

/// Clamps each integer in `object` to the range that the variant its
/// `action` selects declares for it, recording `<prefix><property>`.
fn clamp(
    object: &mut Map<String, Value>,
    actions: &Actions,
    prefix: &str,
    repaired: &mut Vec<String>,
) {
    let Some(properties) = object
        .get("action")
        .and_then(Value::as_str)
        .and_then(|action| actions.get(action))
    else {
        return;
    };
    for (key, value) in object.iter_mut() {
        let (Some(number), Some(bounds)) = (value.as_i64(), properties.get(key)) else {
            continue;
        };
        let minimum = bounds.minimum.unwrap_or(i64::MIN);
        let maximum = bounds.maximum.unwrap_or(i64::MAX);
        if minimum <= maximum && !(minimum..=maximum).contains(&number) {
            *value = Value::from(number.clamp(minimum, maximum));
            repaired.push(format!("{prefix}{key}"));
        }
    }
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
            let Some(arguments) = function.get("arguments").and_then(Value::as_str) else {
                continue;
            };
            if let Some((fixed, repaired)) = watched.repair(&name, arguments) {
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
        assert_eq!(watched.fields("shell"), Some(BTreeSet::from(["request"])));
        assert_eq!(
            watched.fields("read_file"),
            None,
            "options accepts a string"
        );
        assert!(ObjectArgs::from_request(&serde_json::json!({"messages": []})).is_empty());
    }

    #[test]
    fn a_stringified_object_is_parsed_back_and_nothing_else_is_touched() {
        let watched = ObjectArgs::from_request(&fx_tools());
        let repair = |args: &str| watched.repair("shell", args);
        let (fixed, repaired) =
            repair(r#"{"request":"{\"action\": \"run\", \"command\": \"ls\"}"}"#).unwrap();
        assert_eq!(repaired, ["request"]);
        let fixed: Value = serde_json::from_str(&fixed).unwrap();
        assert_eq!(fixed["request"]["command"], "ls");
        // Already an object, a string that is not a JSON object, not JSON at all.
        assert!(repair(r#"{"request":{"action":"run"}}"#).is_none());
        assert!(repair(r#"{"request":"ls -la"}"#).is_none());
        assert!(repair(r#"{"request":"[1, 2]"}"#).is_none());
        assert!(repair("not json").is_none());
    }

    #[test]
    fn a_command_without_its_action_gets_the_one_that_takes_a_command() {
        let watched = ObjectArgs::from_request(&fx_tools());
        assert_eq!(watched.command_action("shell"), Some("run"));
        assert_eq!(watched.command_action("read_file"), None);
        let fixed = |args: &str| {
            watched
                .repair("shell", args)
                .map(|(fixed, fields)| (serde_json::from_str::<Value>(&fixed).unwrap(), fields))
        };
        let (flat, fields) = fixed(r#"{"command":"ls"}"#).unwrap();
        assert_eq!(flat, serde_json::json!({"action": "run", "command": "ls"}));
        assert_eq!(fields, ["action"]);
        let (nested, fields) = fixed(r#"{"request":{"command":"ls"}}"#).unwrap();
        assert_eq!(nested["request"]["action"], "run");
        assert_eq!(fields, ["request.action"]);
        let (both, fields) = fixed(r#"{"request":"{\"command\":\"ls\"}"}"#).unwrap();
        assert_eq!(both["request"]["action"], "run");
        assert_eq!(fields, ["request", "request.action"]);
        assert!(fixed(r#"{"action":"run","command":"ls"}"#).is_none());
        assert!(fixed(r#"{"action":"poll"}"#).is_none());
        assert!(fixed(r#"{"command":["ls"]}"#).is_none());
        // Two actions that take a command: no way to tell which was meant.
        let two = serde_json::json!({"tools": [{"type": "function", "function": {"name": "shell", "parameters": {"type": "object", "properties": {"request": {"oneOf": [
            {"type": "object", "properties": {"action": {"const": "run"}, "command": {"type": "string"}}},
            {"type": "object", "properties": {"action": {"const": "spawn"}, "command": {"type": "string"}}}
        ]}}}}}]});
        assert_eq!(ObjectArgs::from_request(&two).command_action("shell"), None);
    }

    #[test]
    fn fx_0_0_14_shell_calls_without_an_action_are_repaired() {
        let tools: Value =
            serde_json::from_str(include_str!("../testdata/fx-0.0.14-shell.json")).unwrap();
        let watched = ObjectArgs::from_request(&tools);
        assert_eq!(watched.fields("shell"), Some(BTreeSet::from(["request"])));
        assert_eq!(watched.command_action("shell"), Some("run"));
        let fixed = |args: &str| {
            watched
                .repair("shell", args)
                .map(|(fixed, fields)| (serde_json::from_str::<Value>(&fixed).unwrap(), fields))
        };
        let (flat, fields) = fixed(r#"{"command":"ls"}"#).unwrap();
        assert_eq!(flat, serde_json::json!({"action": "run", "command": "ls"}));
        assert_eq!(fields, ["action"]);
    }

    #[test]
    fn properties_beside_the_request_are_moved_into_it_when_its_variant_takes_them() {
        let tools: Value =
            serde_json::from_str(include_str!("../testdata/fx-0.0.14-shell.json")).unwrap();
        let watched = ObjectArgs::from_request(&tools);
        let fixed = |args: &str| {
            watched
                .repair("shell", args)
                .map(|(fixed, fields)| (serde_json::from_str::<Value>(&fixed).unwrap(), fields))
        };
        let (moved, fields) =
            fixed(r#"{"request":{"action":"run","command":"ls"},"timeout_ms":5000,"cwd":"src"}"#)
                .unwrap();
        assert_eq!(
            moved,
            serde_json::json!({"request": {"action": "run", "command": "ls", "timeout_ms": 5000, "cwd": "src"}})
        );
        assert_eq!(fields, ["request.timeout_ms", "request.cwd"]);
        // The missing action is added first, and selects the variant.
        let (moved, fields) = fixed(r#"{"request":{"command":"ls"},"timeout_ms":5000}"#).unwrap();
        assert_eq!(moved["request"]["action"], "run");
        assert_eq!(moved["request"]["timeout_ms"], 5000);
        assert_eq!(fields, ["request.action", "request.timeout_ms"]);
        // A property the variant does not take, or one the request already holds.
        assert!(
            fixed(r#"{"request":{"action":"run","command":"ls"},"timeout_ms":1,"x":1}"#).is_none()
        );
        assert!(
            fixed(r#"{"request":{"action":"run","command":"ls","cwd":"a"},"cwd":"b"}"#).is_none()
        );
        // `chars` belongs to `interact`, not `run`.
        assert!(fixed(r#"{"request":{"action":"run","command":"ls"},"chars":"y"}"#).is_none());
    }

    #[test]
    fn integers_outside_their_variants_range_are_clamped_to_it() {
        let tools: Value =
            serde_json::from_str(include_str!("../testdata/fx-0.0.14-shell.json")).unwrap();
        let watched = ObjectArgs::from_request(&tools);
        let fixed = |args: &str| {
            watched
                .repair("shell", args)
                .map(|(fixed, fields)| (serde_json::from_str::<Value>(&fixed).unwrap(), fields))
        };
        let (nested, fields) = fixed(
            r#"{"request":{"action":"run","command":"./verify --all","yield_time_ms":120000}}"#,
        )
        .unwrap();
        assert_eq!(nested["request"]["yield_time_ms"], 30000);
        assert_eq!(fields, ["request.yield_time_ms"]);
        let (flat, fields) =
            fixed(r#"{"action":"run","command":"ls","yield_time_ms":-5,"timeout_ms":0}"#).unwrap();
        assert_eq!(flat["yield_time_ms"], 0);
        assert_eq!(flat["timeout_ms"], 1);
        assert_eq!(fields, ["yield_time_ms", "timeout_ms"]);
        // Moved beside the request first, then clamped.
        let (moved, fields) =
            fixed(r#"{"request":{"action":"run","command":"ls"},"yield_time_ms":90000}"#).unwrap();
        assert_eq!(moved["request"]["yield_time_ms"], 30000);
        assert_eq!(fields, ["request.yield_time_ms", "request.yield_time_ms"]);
        // In range, not an integer, or no range declared: untouched.
        assert!(
            fixed(r#"{"request":{"action":"run","command":"ls","yield_time_ms":30000}}"#).is_none()
        );
        assert!(
            fixed(r#"{"request":{"action":"run","command":"ls","yield_time_ms":"90000"}}"#)
                .is_none()
        );
        assert!(
            fixed(r#"{"request":{"action":"run","command":"ls","timeout_ms":999999}}"#).is_none()
        );
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
