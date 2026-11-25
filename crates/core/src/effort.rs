//! The reasoning effort the engine is asked for: the session's role's
//! thinking level, lowered one step for the session after a reply that
//! reasoned past the output limit. The orchestrator sets the dial before
//! each session; the engine proxy reads it on every request.

use std::sync::{Arc, Mutex, PoisonError};

/// The `reasoning_effort` a thinking level asks the engine for; `None`
/// leaves the engine's default (unset, `off`, `none`, `auto`).
pub fn reasoning_effort(thinking: Option<&str>) -> Option<&str> {
    match thinking.map(str::trim) {
        None | Some("" | "off" | "none" | "auto") => None,
        Some(level) => Some(level),
    }
}

/// What a session's requests ask for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEffort {
    pub effort: String,
    /// One step below the session's level: it replaces what the harness
    /// sends, instead of filling in a request that names none.
    pub lowered: bool,
}

impl SessionEffort {
    /// The effort of a session at `thinking`, one step lower when
    /// `step_down` asks for it and the level has a lower step; `None` when
    /// the level asks for no effort.
    pub fn for_session(thinking: Option<&str>, step_down: bool) -> Option<Self> {
        let configured = reasoning_effort(thinking)?;
        let lowered = step_down && lower(configured) != configured;
        let effort = if lowered {
            lower(configured)
        } else {
            configured
        };
        Some(Self {
            effort: effort.to_owned(),
            lowered,
        })
    }
}

/// The current session's effort, shared by the orchestrator, which sets
/// it, and the engine proxy, which reads it.
#[derive(Clone, Debug, Default)]
pub struct EffortDial(Arc<Mutex<Option<SessionEffort>>>);

impl EffortDial {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, effort: Option<SessionEffort>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = effort;
    }

    /// The current session's effort; `None` when it asks for none.
    pub fn current(&self) -> Option<SessionEffort> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// One step down: `high` (and above) to `medium`, `medium` to `low`. `low`
/// and anything unknown stay as they are.
pub fn lower(effort: &str) -> &str {
    match effort {
        "xhigh" | "max" | "high" => "medium",
        "medium" => "low",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn effort(thinking: &str, step_down: bool) -> Option<(String, bool)> {
        SessionEffort::for_session(Some(thinking), step_down).map(|e| (e.effort, e.lowered))
    }

    #[test]
    fn a_stepped_down_session_asks_for_one_step_less() {
        for (level, expected) in [
            ("medium", "low"),
            ("high", "medium"),
            ("xhigh", "medium"),
            ("max", "medium"),
        ] {
            assert_eq!(effort(level, false), Some((level.to_owned(), false)));
            assert_eq!(effort(level, true), Some((expected.to_owned(), true)));
        }
    }

    /// `low` and `minimal` have no lower step: such a session is not
    /// lowered, and `off` asks for no effort at all.
    #[test]
    fn a_level_without_a_lower_step_is_not_lowered() {
        assert_eq!(effort("low", true), Some(("low".to_owned(), false)));
        assert_eq!(effort("minimal", true), Some(("minimal".to_owned(), false)));
        assert_eq!(effort("off", true), None);
        assert_eq!(SessionEffort::for_session(None, true), None);
    }

    #[test]
    fn the_proxy_reads_what_the_orchestrator_set() {
        let dial = EffortDial::new();
        let proxy_side = dial.clone();
        assert_eq!(proxy_side.current(), None);
        let high = SessionEffort::for_session(Some("high"), false);
        dial.set(high.clone());
        assert_eq!(proxy_side.current(), high);
        dial.set(None);
        assert_eq!(proxy_side.current(), None);
    }

    #[test]
    fn a_thinking_level_becomes_a_reasoning_effort_unless_off() {
        assert_eq!(reasoning_effort(Some("medium")), Some("medium"));
        assert_eq!(reasoning_effort(Some(" high ")), Some("high"));
        assert_eq!(reasoning_effort(Some("off")), None);
        assert_eq!(reasoning_effort(Some("auto")), None);
        assert_eq!(reasoning_effort(None), None);
    }
}
