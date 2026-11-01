//! The context budget of a session: a share of its model's window, under
//! one cap for the run once the engine has run out of memory, lowered and
//! raised again as the engine holds.

use super::Ledger;
use crate::role::Role;

/// A cap this close to the largest configured budget is lifted rather than
/// raised.
const RAISE_STEP_MIN: u64 = 4096;

/// A raise of the run's cap that a held session made due.
#[derive(Debug, PartialEq, Eq)]
pub struct CapRaise {
    /// The cap after it; `None` when it reached the largest budget.
    pub cap: Option<u64>,
    /// The sessions in a row held before it.
    pub clean_sessions: u32,
    /// The ceiling it went past, if it was tried past one (ADR 0014).
    pub past_ceiling: Option<u64>,
}

impl Ledger {
    /// Lowers the cap to `cap`, the headroom share of what the engine said
    /// it can hold, and returns the cap now. When the raise before it
    /// overflowed, what the engine holds is what it holds without passing
    /// pressure: later raises stop there until it is tried again (ADR
    /// 0014), and wait longer.
    pub fn lower_cap(&mut self, cap: u64) -> u64 {
        if self.context_cap.is_none_or(|current| cap < current) {
            self.context_cap = Some(cap);
        }
        self.cap_clean_sessions = 0;
        if std::mem::take(&mut self.cap_raised) {
            self.cap_failed_raises += 1;
            self.cap_ceiling = Some(self.cap_ceiling.map_or(cap, |c| c.max(cap)));
        }
        self.context_cap.unwrap_or(cap)
    }

    /// A session ran under critical memory pressure: no time to raise the
    /// cap, so the count of sessions held starts again.
    pub fn note_cap_pressed(&mut self) {
        self.cap_clean_sessions = 0;
    }

    /// Counts a session the engine held at the cap and, after
    /// `recover_sessions` of them in a row, raises the cap halfway to the
    /// largest configured budget, or to the ceiling a failed raise set; held
    /// at that ceiling for twice the wait, past it. Each raise that
    /// overflows doubles the wait, up to four times; the raise after one the
    /// engine held starts it again. `None` when no raise is due.
    pub fn note_cap_held(&mut self, recover_sessions: u32) -> Option<CapRaise> {
        let cap = self.context_cap?;
        if recover_sessions == 0 {
            return None;
        }
        self.cap_clean_sessions += 1;
        let clean = self.cap_clean_sessions;
        let waits = clean / raise_wait(recover_sessions, self.cap_failed_raises);
        let full = self.largest_budget_tokens();
        let climb = Climb {
            cap,
            full,
            ceiling: self.cap_ceiling,
        };
        let (raised, ceiling) = climb.after(waits)?;
        let past_ceiling = self.cap_ceiling.filter(|_| ceiling.is_none());
        self.cap_ceiling = ceiling;
        if std::mem::take(&mut self.cap_raised) {
            self.cap_failed_raises = 0;
        }
        self.context_cap = (raised < full).then_some(raised);
        self.cap_clean_sessions = 0;
        self.cap_raised = true;
        Some(CapRaise {
            cap: self.context_cap,
            clean_sessions: clean,
            past_ceiling,
        })
    }

    /// The budget a session on a model with `context_window` is steered
    /// at: the configured share of the window, or less once the engine has
    /// said it cannot hold more.
    pub fn context_budget_tokens(&self, context_window: u64, requested: Option<u64>) -> u64 {
        let wanted = requested.unwrap_or_else(|| self.configured_budget_tokens(context_window));
        self.context_cap.map_or(wanted, |cap| wanted.min(cap))
    }

    /// The budget a session of `role` is steered at: its model's, or the
    /// card's `requested` one, under the run's cap.
    pub fn role_budget_tokens(&self, role: Role, requested: Option<u64>) -> u64 {
        self.context_budget_tokens(self.config.model_for(role).context_window, requested)
    }

    /// The configured budget of `role`'s model: what a prompt must fit. A
    /// card's own budget steers its session, and the cap follows what the
    /// engine says it can hold, lowered and raised again (ADR 0012).
    pub fn model_budget_tokens(&self, role: Role) -> u64 {
        self.configured_budget_tokens(self.config.model_for(role).context_window)
    }

    /// The largest configured budget of the run's models: a cap at or above
    /// it holds no session back.
    pub fn largest_budget_tokens(&self) -> u64 {
        self.configured_budget_tokens(self.config.largest_context_window())
    }

    /// The configured budget of the builder's model: what cards are sized
    /// for.
    pub fn builder_budget_tokens(&self) -> u64 {
        self.configured_budget_tokens(self.config.model_for(Role::Builder).context_window)
    }

    /// The configured share of `context_window`, before any cap.
    fn configured_budget_tokens(&self, context_window: u64) -> u64 {
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a context window times a fraction is far below 2^53 and floors to a positive u64"
        )]
        let budget = (context_window as f64 * self.params.context_budget).floor() as u64;
        budget
    }
}

/// Sessions the engine must hold before a lowered budget is raised: the
/// configured count, doubled per raise that failed, at most four times it.
fn raise_wait(recover_sessions: u32, failed_raises: u32) -> u32 {
    recover_sessions.saturating_mul(1 << failed_raises.min(2))
}

/// A raise from `cap` toward `full`, the largest configured budget, under
/// `ceiling` when a failed raise set one.
#[derive(Clone, Copy, Debug)]
struct Climb {
    cap: u64,
    full: u64,
    ceiling: Option<u64>,
}

impl Climb {
    /// The budget after a raise: halfway to `full`, or to `ceiling` when that
    /// is lower, and all the way when less than [`RAISE_STEP_MIN`] is left.
    /// `None` when `cap` is already there.
    fn raised(self) -> Option<u64> {
        let target = self.ceiling.map_or(self.full, |c| c.min(self.full));
        if self.cap >= target {
            return None;
        }
        let raised = self.cap + (target - self.cap) / 2;
        let raised = if target - raised < RAISE_STEP_MIN {
            target
        } else {
            raised
        };
        Some(raised)
    }

    /// The cap, and the ceiling left, after `cap` was held for `waits` whole
    /// waits in a row. At a ceiling a failed raise set, the engine is tried
    /// past it after two waits: that ceiling is what the engine held under
    /// the memory pressure of then, and memory freed since is only found by
    /// trying. A raise it overflows sets the ceiling again. `None` when
    /// nothing is due.
    fn after(self, waits: u32) -> Option<(u64, Option<u64>)> {
        if waits == 0 {
            return None;
        }
        if let Some(raised) = self.raised() {
            return Some((raised, self.ceiling));
        }
        let unbounded = Self {
            ceiling: None,
            ..self
        };
        if self.ceiling.is_some() && waits >= 2 {
            return unbounded.raised().map(|raised| (raised, None));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::tests::{config, role_model};
    use crate::params::RunParams;

    /// A run on a 131072 window: 85196 is its largest budget.
    fn ledger() -> Ledger {
        Ledger::new(
            "t".into(),
            "b".into(),
            "s".into(),
            "0".into(),
            config(),
            RunParams::default(),
            vec![],
        )
    }

    /// Holds `sessions` at the cap, and returns the raises they made due.
    fn hold(ledger: &mut Ledger, sessions: u32) -> Vec<CapRaise> {
        (0..sessions)
            .filter_map(|_| ledger.note_cap_held(4))
            .collect()
    }

    /// The Digger run of 2026-10-30: the engine said 73024 while memory was
    /// short, and 58419 held for hours after the pressure was gone, with
    /// sessions reaching 77257 tokens unharmed.
    #[test]
    fn a_ceiling_held_for_two_waits_is_tried_past() {
        let mut ledger = ledger();
        ledger.lower_cap(45_722);
        let raises = hold(&mut ledger, 4);
        assert_eq!(raises.last().map(|r| r.cap), Some(Some(65_459)));
        // That raise overflows: the ceiling is set, the wait doubles to 8.
        assert_eq!(ledger.lower_cap(58_419), 58_419);
        assert_eq!(ledger.cap_ceiling, Some(58_419));
        assert!(hold(&mut ledger, 15).is_empty(), "twice the wait of 8");
        assert_eq!(
            hold(&mut ledger, 1),
            [CapRaise {
                cap: Some(71_807),
                clean_sessions: 16,
                past_ceiling: Some(58_419),
            }]
        );
        assert_eq!(ledger.cap_ceiling, None, "dropped once tried past");
        // That one overflows too, lower: the ceiling is set from it alone.
        ledger.lower_cap(55_000);
        assert_eq!(ledger.cap_ceiling, Some(55_000));
        assert_eq!(ledger.cap_failed_raises, 2);
    }

    #[test]
    fn under_its_ceiling_a_cap_climbs_to_it_then_waits_two_waits() {
        let mut ledger = ledger();
        ledger.lower_cap(40_000);
        assert_eq!(hold(&mut ledger, 4).len(), 1);
        ledger.lower_cap(50_000); // the raise overflows: ceiling 50000
        ledger.lower_cap(45_000); // a later overflow, under it
        assert_eq!(
            (ledger.context_cap, ledger.cap_ceiling),
            (Some(45_000), Some(50_000))
        );
        let to_ceiling = CapRaise {
            cap: Some(50_000),
            clean_sessions: 8,
            past_ceiling: None,
        };
        assert_eq!(hold(&mut ledger, 8), [to_ceiling], "less than 4096 short");
        assert!(hold(&mut ledger, 15).is_empty(), "held at the ceiling");
        let past = CapRaise {
            cap: Some(67_598),
            clean_sessions: 16,
            past_ceiling: Some(50_000),
        };
        assert_eq!(hold(&mut ledger, 1), [past]);
    }

    #[test]
    fn critical_pressure_starts_the_count_again() {
        let mut ledger = ledger();
        ledger.lower_cap(45_000);
        assert!(hold(&mut ledger, 3).is_empty());
        ledger.note_cap_pressed();
        assert!(hold(&mut ledger, 3).is_empty());
        assert_eq!(hold(&mut ledger, 1).len(), 1);
    }

    #[test]
    fn no_raise_without_a_cap_or_a_wait() {
        let mut ledger = ledger();
        assert_eq!(ledger.note_cap_held(4), None);
        ledger.lower_cap(45_000);
        assert_eq!(ledger.note_cap_held(0), None);
        assert_eq!(ledger.cap_clean_sessions, 0);
    }

    #[test]
    fn raises_go_halfway_and_stop_at_the_ceiling_a_failed_raise_set() {
        let full = 85_196;
        let raised = |cap, ceiling| Climb { cap, full, ceiling }.raised();
        // No ceiling: halfway to the configured budget, then all the way.
        assert_eq!(raised(49_190, None), Some(67_193));
        assert_eq!(raised(83_000, None), Some(85_196));
        // The Wild West run: the engine held 86016 when a raise overflowed,
        // so 68812 is the ceiling, and a passing low reading climbs back to it.
        let ceiling = Some(68_812);
        assert_eq!(raised(38_215, ceiling), Some(53_513));
        assert_eq!(raised(66_000, ceiling), Some(68_812));
        assert_eq!(raised(68_812, ceiling), None);
        assert_eq!(raised(70_000, ceiling), None);
        // A ceiling above the configured budget does not lift it further,
        // nor is it dropped.
        let above = Climb {
            cap: 60_000,
            full,
            ceiling: Some(90_000),
        };
        assert_eq!(above.raised(), Some(72_598));
        assert_eq!(above.after(2), Some((72_598, Some(90_000))));
    }

    #[test]
    fn the_wait_before_a_raise_doubles_at_most_twice() {
        assert_eq!(raise_wait(4, 0), 4);
        assert_eq!(raise_wait(4, 1), 8);
        assert_eq!(raise_wait(4, 2), 16);
        assert_eq!(raise_wait(4, 7), 16);
        assert_eq!(raise_wait(0, 3), 0);
    }

    #[test]
    fn budgets_follow_each_window_under_one_cap_for_the_run() {
        let mut config = config();
        config
            .role_models
            .insert(Role::Reviewer, role_model("r", 65536));
        config
            .role_models
            .insert(Role::FinalReviewer, role_model("f", 262_144));
        let mut ledger = Ledger::new(
            "t".into(),
            "b".into(),
            "s".into(),
            "0".into(),
            config,
            RunParams::default(),
            vec![],
        );
        assert_eq!(ledger.context_budget_tokens(65536, None), 42598);
        assert_eq!(ledger.context_budget_tokens(131_072, None), 85196);
        assert_eq!(ledger.largest_budget_tokens(), 170_393);
        ledger.context_cap = Some(49_190);
        assert_eq!(ledger.context_budget_tokens(65536, None), 42598);
        assert_eq!(ledger.context_budget_tokens(262_144, None), 49_190);
        ledger.context_cap = Some(30_000);
        assert_eq!(ledger.context_budget_tokens(65536, None), 30_000);
    }

    /// A prompt is checked against its model's budget, before a card's
    /// budget and the cap.
    #[test]
    fn a_role_s_budget_is_capped_and_its_model_s_is_not() {
        let mut config = config();
        config
            .role_models
            .insert(Role::Reviewer, role_model("r", 65536));
        let mut ledger = Ledger::new(
            "t".into(),
            "b".into(),
            "s".into(),
            "0".into(),
            config,
            RunParams::default(),
            vec![],
        );
        ledger.context_cap = Some(30_000);
        // 0.65 of the reviewer's 65536 and of the run's 131072.
        let (reviewer, run) = (42598, 85196);
        for (role, requested, capped, model) in [
            (Role::Reviewer, None, 30_000, reviewer),
            (Role::Builder, None, 30_000, run),
            (Role::Builder, Some(20_000), 20_000, run),
            (Role::Builder, Some(40_000), 30_000, run),
        ] {
            assert_eq!(
                ledger.role_budget_tokens(role, requested),
                capped,
                "{role} {requested:?}"
            );
            assert_eq!(ledger.model_budget_tokens(role), model, "{role}");
        }
    }

    #[test]
    fn the_largest_budget_is_the_run_s_when_its_window_is_the_largest() {
        let mut config = config();
        config
            .role_models
            .insert(Role::Reviewer, role_model("r", 65536));
        let ledger = Ledger::new(
            "t".into(),
            "b".into(),
            "s".into(),
            "0".into(),
            config,
            RunParams::default(),
            vec![],
        );
        assert_eq!(ledger.largest_budget_tokens(), 85196);
        assert_eq!(ledger.builder_budget_tokens(), 85196);
    }
}
