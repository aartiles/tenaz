//! The context budget of a session: a share of its model's window, under
//! one cap for the run once the engine has run out of memory.

use super::Ledger;
use crate::role::Role;

impl Ledger {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::tests::{config, role_model};
    use crate::params::RunParams;

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
