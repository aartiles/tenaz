//! The reasoning effort the engine is asked for, lowered one step for the
//! session after a reply that reasoned past the output limit. The
//! orchestrator turns the dial before each session; the engine proxy reads
//! it on every request.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether the next requests use a lower reasoning effort than configured.
#[derive(Clone, Debug, Default)]
pub struct EffortDial(Arc<AtomicBool>);

impl EffortDial {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_lowered(&self, lowered: bool) {
        self.0.store(lowered, Ordering::SeqCst);
    }

    pub fn lowered(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// The effort to ask for, given the configured one.
    pub fn effort<'a>(&self, configured: &'a str) -> &'a str {
        if self.lowered() {
            lower(configured)
        } else {
            configured
        }
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

    #[test]
    fn the_dial_lowers_one_step_while_it_is_turned() {
        let dial = EffortDial::new();
        assert_eq!(dial.effort("medium"), "medium");
        let proxy_side = dial.clone();
        dial.set_lowered(true);
        assert_eq!(proxy_side.effort("medium"), "low");
        assert_eq!(proxy_side.effort("high"), "medium");
        assert_eq!(proxy_side.effort("low"), "low");
        dial.set_lowered(false);
        assert_eq!(proxy_side.effort("high"), "high");
    }
}
