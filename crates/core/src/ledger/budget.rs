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
