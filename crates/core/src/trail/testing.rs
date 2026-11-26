//! Events and trails for tests elsewhere in the crate.

use crate::session::{SessionEvent, StopReason, Usage};

/// A reply of the model with `text`, failed when `error` is given.
pub(crate) fn reply(text: &str, error: Option<&str>) -> SessionEvent {
    SessionEvent::MessageEnd {
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        error_message: error.map(str::to_owned),
        thinking_chars: 0,
        text: text.into(),
        server: None,
        sent: None,
    }
}

/// A tool call that starts.
pub(crate) fn start(id: &str, name: &str, args: serde_json::Value) -> SessionEvent {
    SessionEvent::ToolCallStart {
        id: id.into(),
        name: name.into(),
        args,
        effect: None,
    }
}

/// A call that ends.
pub(crate) fn end(id: &str, result: &str, is_error: bool) -> SessionEvent {
    SessionEvent::ToolCallEnd {
        id: id.into(),
        name: "tool".into(),
        result_text: result.into(),
        is_error,
    }
}

/// A `write` call on `path` that starts and ends without an error.
pub(crate) fn write(id: &str, path: &str) -> [SessionEvent; 2] {
    [
        start(id, "write", serde_json::json!({ "path": path })),
        end(id, "wrote", false),
    ]
}

/// A shell call that runs `command` and ends with `result`.
pub(crate) fn run(id: &str, command: &str, result: &str, failed: bool) -> [SessionEvent; 2] {
    [
        start(id, "shell", serde_json::json!({ "command": command })),
        end(id, result, failed),
    ]
}

/// The trail of `events`.
pub(crate) fn trail(events: &[SessionEvent]) -> super::Trail {
    let mut trail = super::Trail::default();
    for event in events {
        trail.observe(event);
    }
    trail
}
