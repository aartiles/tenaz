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

mod stream;

pub(crate) use stream::StreamRepair;

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
    pub(crate) fn repair(&self, tool: &str, arguments: &str) -> Option<(String, Vec<String>)> {
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

impl ToolArgs {
    /// Calls `repair` on each object that a pass repairs, with the prefix
    /// that names it and the actions its variants can take: each watched
    /// field that holds an object, or the call itself when it holds no
    /// watched field.
    fn each_target(
        &self,
        object: &mut Map<String, Value>,
        mut repair: impl FnMut(&mut Map<String, Value>, &str, &[&Actions]),
    ) {
        if self.fields.keys().any(|f| object.contains_key(f)) {
            for (field, actions) in &self.fields {
                if let Some(Value::Object(inner)) = object.get_mut(field) {
                    repair(inner, &format!("{field}."), &[actions]);
                }
            }
        } else {
            let actions: Vec<&Actions> = self.fields.values().collect();
            repair(object, "", &actions);
        }
    }
}

/// One repair of a call's arguments. It records each property it changed
/// in `repaired`, prefixed with the watched field it is in, if any.
type Pass = fn(&mut Map<String, Value>, &ToolArgs, &mut Vec<String>);

/// The repairs, in order: an object sent as text must be parsed before its
/// `action` can be added, the `action` selects the variant that decides
/// what is moved in, and a property moved in is clamped there.
const PASSES: [Pass; 4] = [
    parse_stringified,
    add_command_action,
    move_stray_properties,
    clamp_integers,
];

/// The arguments with every pass applied, and the repaired fields; `None`
/// when nothing needed repair (or the arguments are not a JSON object).
fn repair_arguments(arguments: &str, tool: &ToolArgs) -> Option<(String, Vec<String>)> {
    let mut object: Map<String, Value> = serde_json::from_str(arguments).ok()?;
    let mut repaired = Vec::new();
    for pass in PASSES {
        pass(&mut object, tool, &mut repaired);
    }
    if repaired.is_empty() {
        return None;
    }
    Some((serde_json::to_string(&object).ok()?, repaired))
}

/// Parses each watched field that holds a JSON object as text back into
/// that object.
fn parse_stringified(object: &mut Map<String, Value>, tool: &ToolArgs, repaired: &mut Vec<String>) {
    for field in tool.fields.keys() {
        let Some(Value::String(text)) = object.get(field) else {
            continue;
        };
        if let Ok(inner @ Value::Object(_)) = serde_json::from_str::<Value>(text) {
            object.insert(field.clone(), inner);
            repaired.push(field.clone());
        }
    }
}

/// Adds the tool's `command_action` to a `command` sent without an
/// `action`, at the top level (when no watched field is there) or in a
/// watched field.
fn add_command_action(
    object: &mut Map<String, Value>,
    tool: &ToolArgs,
    repaired: &mut Vec<String>,
) {
    let Some(action) = &tool.command_action else {
        return;
    };
    tool.each_target(object, |target, prefix, _| {
        if lacks_action(target) {
            target.insert("action".to_owned(), Value::String(action.clone()));
            repaired.push(format!("{prefix}action"));
        }
    });
}

/// A `command` text without an `action`.
fn lacks_action(object: &Map<String, Value>) -> bool {
    !object.contains_key("action") && object.get("command").is_some_and(Value::is_string)
}

/// Moves the properties sent beside a watched field into it, when the
/// variant its `action` selects takes them all.
fn move_stray_properties(
    object: &mut Map<String, Value>,
    tool: &ToolArgs,
    repaired: &mut Vec<String>,
) {
    for (field, actions) in &tool.fields {
        let Some(stray) = strays(object, tool, field, actions) else {
            continue;
        };
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
}

/// The properties beside `field`, when there are some, the variant its
/// `action` selects takes them all, and `field` holds none of them yet.
fn strays(
    object: &Map<String, Value>,
    tool: &ToolArgs,
    field: &str,
    actions: &Actions,
) -> Option<Vec<String>> {
    let Some(Value::Object(inner)) = object.get(field) else {
        return None;
    };
    let properties = variant(inner, actions)?;
    let stray: Vec<String> = object
        .keys()
        .filter(|key| !tool.fields.contains_key(*key))
        .cloned()
        .collect();
    let fits = |key: &String| properties.contains_key(key) && !inner.contains_key(key);
    (!stray.is_empty() && stray.iter().all(fits)).then_some(stray)
}

/// The properties of the variant that `object`'s `action` selects.
fn variant<'a>(
    object: &Map<String, Value>,
    actions: &'a Actions,
) -> Option<&'a BTreeMap<String, Bounds>> {
    object
        .get("action")
        .and_then(Value::as_str)
        .and_then(|action| actions.get(action))
}

/// Clamps integers outside their variant's range: in the watched fields,
/// or at the top level when the call holds none.
fn clamp_integers(object: &mut Map<String, Value>, tool: &ToolArgs, repaired: &mut Vec<String>) {
    tool.each_target(object, |target, prefix, actions| {
        for actions in actions {
            clamp(target, actions, prefix, repaired);
        }
    });
}

/// Clamps each integer in `object` to the range that the variant its
/// `action` selects declares for it, recording `<prefix><property>`.
fn clamp(
    object: &mut Map<String, Value>,
    actions: &Actions,
    prefix: &str,
    repaired: &mut Vec<String>,
) {
    let Some(properties) = variant(object, actions) else {
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

/// fx 0.0.14's `shell` tool, cut down: `request` is a `oneOf` of objects.
#[cfg(test)]
fn fx_tools() -> Value {
    let run = serde_json::json!({"type": "object", "properties": {"action": {"type": "string", "enum": ["run"]}, "command": {"type": "string"}}, "required": ["action", "command"]});
    let poll = serde_json::json!({"type": "object", "properties": {"action": {"type": "string", "enum": ["poll"]}}});
    serde_json::json!({"tools": [
        {"type": "function", "function": {"name": "shell", "parameters": {"type": "object", "properties": {"request": {"oneOf": [run, poll]}}}}},
        {"type": "function", "function": {"name": "read_file", "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "options": {"anyOf": [{"type": "object"}, {"type": "string"}]}}}}}
    ]})
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for untouched in [
            r#"{"request":{"action":"run"}}"#,
            r#"{"request":"ls -la"}"#,
            r#"{"request":"[1, 2]"}"#,
            "not json",
        ] {
            assert_eq!(repair(untouched), None, "{untouched}");
        }
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
        for untouched in [
            r#"{"action":"run","command":"ls"}"#,
            r#"{"action":"poll"}"#,
            r#"{"command":["ls"]}"#,
            // The watched field is there, though not an object: the call
            // is not repaired at the top level.
            r#"{"request":"ls -la","command":"ls"}"#,
        ] {
            assert_eq!(fixed(untouched), None, "{untouched}");
        }
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
        // A property the variant does not take, or one the request already
        // holds; `chars` belongs to `interact`, not `run`.
        for untouched in [
            r#"{"request":{"action":"run","command":"ls"},"timeout_ms":1,"x":1}"#,
            r#"{"request":{"action":"run","command":"ls","cwd":"a"},"cwd":"b"}"#,
            r#"{"request":{"action":"run","command":"ls"},"chars":"y"}"#,
        ] {
            assert_eq!(fixed(untouched), None, "{untouched}");
        }
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
        for untouched in [
            r#"{"request":{"action":"run","command":"ls","yield_time_ms":30000}}"#,
            r#"{"request":{"action":"run","command":"ls","yield_time_ms":"90000"}}"#,
            r#"{"request":{"action":"run","command":"ls","timeout_ms":999999}}"#,
            r#"{"request":"x","action":"run","command":"ls","yield_time_ms":-5}"#,
        ] {
            assert_eq!(fixed(untouched), None, "{untouched}");
        }
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
}
