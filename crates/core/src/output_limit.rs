//! Whether the output limit a run pins (`maxOutputTokens`) fits the
//! windows of its models (ADR 0041). A reply is written into the same
//! window as the prompt and the session so far: a limit that leaves no
//! room for the prompt is refused, and one that a session at its context
//! budget would not have room for is warned about.

use crate::ledger::ModelConfig;
use crate::params::RunParams;
use crate::ports::SessionModel;
use crate::role::Role;

/// What `init` and `run` say about the run's pinned output limit: `Err`
/// with why it does not fit, or the warnings for a limit that fits only
/// while a session is small. A run that pins none gets neither.
pub fn check(params: &RunParams, config: &ModelConfig) -> Result<Vec<String>, String> {
    let Some(limit) = params.max_output_tokens.map(std::num::NonZeroU64::get) else {
        return Ok(Vec::new());
    };
    let mut refusals = Vec::new();
    let mut warnings = Vec::new();
    for (model, roles) in models_and_roles(config) {
        let window = model.context_window;
        let reserve = params.harness_reserve_tokens;
        let budget = params.budget_tokens(window);
        let of = format!("{}'s window of {window} tokens ({roles})", model.name);
        if limit.saturating_add(reserve) >= window {
            refusals.push(format!(
                "params.maxOutputTokens {limit} leaves no room in {of} for a prompt and the \
                 harness's reserve of {reserve} (params.harnessReserveTokens); set it below {}",
                window.saturating_sub(reserve)
            ));
        } else if budget >= window {
            warnings.push(format!(
                "params.contextBudget {} lets a session fill {of} ({budget} tokens), \
                 leaving no room for a reply of params.maxOutputTokens {limit}. \
                 params.contextBudget at most {} keeps the room",
                params.context_budget,
                room_share(window, limit)
            ));
        } else if limit > window - budget {
            warnings.push(format!(
                "params.maxOutputTokens {limit} does not fit in {of} next to a session at its \
                 context budget of {budget} tokens: such a reply is likely cut at the end of the \
                 window, or the engine refuses the request. params.contextBudget at most {}, or \
                 params.maxOutputTokens at most {}, keeps the room",
                room_share(window, limit),
                window - budget
            ));
        }
    }
    if refusals.is_empty() {
        Ok(warnings)
    } else {
        Err(refusals.join("; "))
    }
}

/// Each model the run's sessions play, once, with the roles that play it
/// (`builder, reviewer`), the run's own model first.
fn models_and_roles(config: &ModelConfig) -> Vec<(SessionModel, String)> {
    let mut models: Vec<(SessionModel, Vec<&str>)> = Vec::new();
    for role in Role::ALL {
        let model = config.model_for(role);
        match models.iter_mut().find(|(m, _)| *m == model) {
            Some((_, roles)) => roles.push(role.as_str()),
            None => models.push((model, vec![role.as_str()])),
        }
    }
    models
        .into_iter()
        .map(|(model, roles)| (model, roles.join(", ")))
        .collect()
}

/// The largest `contextBudget`, to two decimals, that leaves `limit`
/// tokens of `window` for a reply.
fn room_share(window: u64, limit: u64) -> String {
    let hundredths = window.saturating_sub(limit).saturating_mul(100) / window.max(1);
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::ledger::HarnessKind;
    use crate::role_models::RoleModel;

    /// A run on a 131072-token model, its reviewer on a 65536-token one.
    fn config() -> ModelConfig {
        let reviewer = RoleModel {
            model: "engine/small".into(),
            id: "small".into(),
            context_window: 65_536,
            input: vec!["text".into()],
        };
        ModelConfig {
            harness: HarnessKind::Pi,
            harness_version: "1".into(),
            model: "engine/m".into(),
            provider: Some("engine".into()),
            id: "m".into(),
            base_url: None,
            context_window: 131_072,
            input: vec!["text".into()],
            thinking: None,
            role_models: BTreeMap::from([(Role::Reviewer, reviewer)]),
        }
    }

    fn params(sets: &[&str]) -> RunParams {
        let mut params = RunParams::default();
        for pair in sets {
            params.apply_override(pair).unwrap();
        }
        params
    }

    /// A model with one window, so that each boundary is its own.
    fn one_model() -> ModelConfig {
        let mut config = config();
        config.role_models.clear();
        config
    }

    #[test]
    fn a_run_that_pins_no_limit_is_not_checked() {
        assert_eq!(check(&params(&[]), &config()), Ok(Vec::new()));
    }

    #[test]
    fn a_limit_that_fits_every_window_at_its_budget_says_nothing() {
        let fits = params(&["maxOutputTokens=20000"]);
        assert_eq!(check(&fits, &config()), Ok(Vec::new()));
    }

    /// 65536 tokens of a reply leave the run's model 65536, under its
    /// 85196-token budget, and the reviewer's model no room at all; the
    /// run's model, which the other roles play, is not refused.
    #[test]
    fn a_limit_with_no_room_for_a_prompt_is_refused_for_its_model() {
        let refused = check(&params(&["maxOutputTokens=65536"]), &config()).unwrap_err();
        assert_eq!(
            refused,
            "params.maxOutputTokens 65536 leaves no room in engine/small's window of 65536 \
             tokens (reviewer) for a prompt and the harness's reserve of 8192 \
             (params.harnessReserveTokens); set it below 57344"
        );
    }

    /// Every model with no room is named.
    #[test]
    fn every_model_without_room_is_refused() {
        let refused = check(&params(&["maxOutputTokens=125000"]), &config()).unwrap_err();
        assert!(
            refused.starts_with("params.maxOutputTokens 125000 leaves no room in engine/m's window of 131072 tokens (bootstrap, builder, split, replan, final-reviewer)"),
            "{refused}"
        );
        assert!(
            refused.contains("; params.maxOutputTokens 125000 leaves no room in engine/small's"),
            "{refused}"
        );
    }

    /// The limit and the reserve may not fill the window; one token less
    /// leaves room for a prompt.
    #[test]
    fn the_refusal_starts_where_the_limit_and_the_reserve_fill_the_window() {
        let at = |limit: u64| {
            check(
                &params(&[&format!("maxOutputTokens={limit}")]),
                &one_model(),
            )
        };
        assert!(at(122_880).is_err());
        assert!(at(122_879).is_ok());
    }

    /// The warning starts one token past what the budget leaves of the
    /// window: 131072 - 85196 = 45876.
    #[test]
    fn the_warning_starts_one_token_past_the_budget_s_room() {
        let at = |limit: u64| {
            check(
                &params(&[&format!("maxOutputTokens={limit}")]),
                &one_model(),
            )
        };
        assert_eq!(at(45_876), Ok(Vec::new()));
        assert_eq!(at(45_877).map(|w| w.len()), Ok(1));
    }

    /// The Inkling runs: a 131072-token window with a reply of 65536.
    #[test]
    fn a_limit_past_the_budget_s_room_is_warned_about_with_both_remedies() {
        let warnings = check(&params(&["maxOutputTokens=65536"]), &one_model()).unwrap();
        assert_eq!(
            warnings,
            [
                "params.maxOutputTokens 65536 does not fit in engine/m's window of 131072 tokens \
              (bootstrap, builder, reviewer, split, replan, final-reviewer) next to a session \
              at its context budget of 85196 tokens: such a reply is likely cut at the end of \
              the window, or the engine refuses the request. params.contextBudget at most \
              0.50, or params.maxOutputTokens at most 45876, keeps the room"
            ]
        );
    }

    /// A context budget that fills the window, or more, leaves no room
    /// for any limit: the warning names the budget, and its one remedy
    /// clears it.
    #[test]
    fn a_context_budget_that_fills_the_window_is_the_one_thing_to_lower() {
        for (share, budget) in [("1", 131_072), ("1.2", 157_286)] {
            let set = [
                "maxOutputTokens=1000".to_owned(),
                format!("contextBudget={share}"),
            ];
            let set: Vec<&str> = set.iter().map(String::as_str).collect();
            let warnings = check(&params(&set), &one_model()).unwrap();
            assert_eq!(
                warnings,
                [format!(
                    "params.contextBudget {share} lets a session fill engine/m's window of \
                     131072 tokens (bootstrap, builder, reviewer, split, replan, \
                     final-reviewer) ({budget} tokens), leaving no room for a reply of \
                     params.maxOutputTokens 1000. params.contextBudget at most 0.99 keeps \
                     the room"
                )]
            );
        }
        let lowered = params(&["maxOutputTokens=1000", "contextBudget=0.99"]);
        assert_eq!(check(&lowered, &one_model()), Ok(Vec::new()));
    }

    /// Either remedy the warning gives clears it, whatever the limit.
    #[test]
    fn the_remedies_the_warning_gives_clear_it() {
        for limit in [45_877, 50_000, 65_536, 99_999, 122_000] {
            let set = format!("maxOutputTokens={limit}");
            let warnings = check(&params(&[&set]), &one_model()).unwrap();
            let [warning] = warnings.as_slice() else {
                panic!("{limit}: {warnings:?}");
            };
            let after = |key: &str| {
                let rest = warning.split_once(&format!("{key} at most ")).unwrap().1;
                rest.split(',').next().unwrap().to_owned()
            };
            let budget = format!("contextBudget={}", after("params.contextBudget"));
            assert_eq!(
                check(&params(&[&set, &budget]), &one_model()),
                Ok(Vec::new()),
                "{budget}"
            );
            let lower = format!("maxOutputTokens={}", after("params.maxOutputTokens"));
            assert_eq!(
                check(&params(&[&lower]), &one_model()),
                Ok(Vec::new()),
                "{lower}"
            );
        }
    }
}
