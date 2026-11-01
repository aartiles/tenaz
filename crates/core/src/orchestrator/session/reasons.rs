//! What the run says when a session is rerun or the run stops: the log
//! line and notice of a rerun, the reason it gives up, and why a prompt
//! does not fit its role's model.

use crate::orchestrator::recovery::{FailureCause, GiveUp};
use crate::ports::SessionModel;
use crate::role::Role;
use crate::task::TaskId;
use crate::util::approx_code_tokens;

/// What the operator can do when the engine runs out of memory: in the
/// Digger 3D run waits did not bring back the memory mlx-serve needed, a
/// reload did (ADR 0012).
const OUT_OF_MEMORY_ADVICE: &str =
    "free memory (another program may be using the GPU) or reload the model";

/// Why a session the engine failed is rerun, for the log.
pub(super) fn retry_reason(cause: FailureCause) -> String {
    match cause {
        FailureCause::Harness => "the harness ended before the turn settled".to_owned(),
        FailureCause::Engine => "engine error".to_owned(),
        FailureCause::OutOfMemory => {
            format!("the engine ran out of memory; {OUT_OF_MEMORY_ADVICE}")
        }
    }
}

/// The notice at the first rerun of a session the engine failed; none for
/// a harness that ended on its own.
pub(super) fn retry_notice(cause: FailureCause) -> Option<String> {
    match cause {
        FailureCause::Harness => None,
        FailureCause::Engine => Some(
            "The engine is failing (often memory). Free memory; the run retries the same session."
                .to_owned(),
        ),
        FailureCause::OutOfMemory => Some(format!(
            "The engine ran out of memory: {OUT_OF_MEMORY_ADVICE}. The run retries the same session."
        )),
    }
}

/// Why the run stops rerunning `label`, under the run's `context_cap`.
pub(super) fn give_up_reason(label: &str, why: GiveUp, context_cap: Option<u64>) -> String {
    match why {
        GiveUp::Capacity => format!(
            "sessions still overflow the engine's memory after the context budget was lowered to {} (last: {label})",
            context_cap.unwrap_or(0)
        ),
        GiveUp::Engine {
            sessions,
            cause: FailureCause::OutOfMemory,
        } => format!(
            "{sessions} sessions in a row ended with engine errors, the last one out of memory (last: {label}); {OUT_OF_MEMORY_ADVICE}"
        ),
        GiveUp::Engine { sessions, .. } => {
            format!("{sessions} sessions in a row ended with engine errors (last: {label})")
        }
    }
}

/// The first request of a session with `prompt`, estimated: the prompt at
/// three bytes per token, plus the harness's `reserve`. Over the Digger 3D
/// run, with a reserve of 8192, this was 2.2k to 4.5k tokens high and never
/// low (ADR 0012).
fn first_request_tokens(prompt: &str, reserve: u64) -> u64 {
    approx_code_tokens(prompt) as u64 + reserve
}

/// What a prompt of a `role` session must fit: `budget` tokens of
/// `model`, with the harness's `reserve` on top of the prompt.
pub(super) struct PromptLimit {
    pub role: Role,
    pub model: SessionModel,
    pub budget: u64,
    pub reserve: u64,
}

impl PromptLimit {
    /// Why `prompt`, for `task` when there is one, does not fit; `None`
    /// when it does.
    pub(super) fn over(&self, prompt: &str, task: Option<&TaskId>) -> Option<String> {
        let estimate = first_request_tokens(prompt, self.reserve);
        if estimate <= self.budget {
            return None;
        }
        let role = self.role;
        let what = task.map_or_else(
            || format!("{role} prompt"),
            |task| format!("{role} prompt for {task}"),
        );
        Some(format!(
            "the {what}, {} bytes, is about {estimate} tokens with the harness's reserve, over the {role}'s budget of {}; {}",
            prompt.len(),
            self.budget,
            self.what_to_change()
        ))
    }

    /// Why the run stops at `label`, whose prompt of `prompt_bytes` the
    /// harness refused: what was refused, and what to change.
    pub(super) fn refused(&self, label: &str, prompt_bytes: usize) -> String {
        format!(
            "the harness refused the {} prompt of {label}, {prompt_bytes} bytes; {}",
            self.role,
            self.what_to_change()
        )
    }

    /// The end both reasons share: the model's name and window, and what
    /// to change.
    fn what_to_change(&self) -> String {
        format!(
            "{}'s window is {} tokens. To go on, {}",
            self.model.name,
            self.model.context_window,
            change_advice(self.role)
        )
    }
}

/// What to change when a `role` prompt does not fit: a review can be made
/// smaller; a model is changed in a new run only.
fn change_advice(role: Role) -> String {
    let larger = format!(
        "give the {role} a model with a larger window in a new run (tenaz init --model-for, --context-window-for)"
    );
    if role == Role::Reviewer {
        format!(
            "lower params.diffShare or params.diffLimitBytes in .tenaz/run.json and run with --resume, or {larger}"
        )
    } else {
        larger
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// mlx-serve refuses a prompt for memory until the model is reloaded
    /// (the Digger 3D run of 2026-10-29): the reason says so.
    #[test]
    fn giving_up_after_the_engine_ran_out_of_memory_says_what_to_do() {
        let why = GiveUp::Engine {
            sessions: 7,
            cause: FailureCause::OutOfMemory,
        };
        assert_eq!(
            give_up_reason("031-reviewer-T003", why, None),
            "7 sessions in a row ended with engine errors, the last one out of memory (last: 031-reviewer-T003); free memory (another program may be using the GPU) or reload the model"
        );
    }

    #[test]
    fn giving_up_after_other_engine_errors_gives_no_memory_advice() {
        let why = GiveUp::Engine {
            sessions: 7,
            cause: FailureCause::Engine,
        };
        assert_eq!(
            give_up_reason("020-reviewer-T003", why, None),
            "7 sessions in a row ended with engine errors (last: 020-reviewer-T003)"
        );
    }

    fn qwen() -> SessionModel {
        SessionModel {
            name: "mlx-serve/Qwen3.8-27B".into(),
            id: "Qwen3.8-27B".into(),
            context_window: 65_536,
            supports_images: false,
        }
    }

    fn limit(role: Role) -> PromptLimit {
        PromptLimit {
            role,
            model: qwen(),
            budget: 8202,
            reserve: 8192,
        }
    }

    /// The reviewer of T001 in the Digger 3D run of 2026-10-29.
    #[test]
    fn a_refused_review_names_its_size_and_window_and_what_to_lower() {
        assert_eq!(
            limit(Role::Reviewer).refused("007-reviewer-T001", 281_465),
            "the harness refused the reviewer prompt of 007-reviewer-T001, 281465 bytes; mlx-serve/Qwen3.8-27B's window is 65536 tokens. To go on, lower params.diffShare or params.diffLimitBytes in .tenaz/run.json and run with --resume, or give the reviewer a model with a larger window in a new run (tenaz init --model-for, --context-window-for)"
        );
    }

    #[test]
    fn a_refused_prompt_of_another_role_asks_for_a_larger_window_only() {
        assert_eq!(
            limit(Role::Builder).refused("005-builder-T001", 300_000),
            "the harness refused the builder prompt of 005-builder-T001, 300000 bytes; mlx-serve/Qwen3.8-27B's window is 65536 tokens. To go on, give the builder a model with a larger window in a new run (tenaz init --model-for, --context-window-for)"
        );
    }

    /// Session 025 of the Digger 3D run: a reviewer prompt of 79670
    /// bytes whose first request was 30245 tokens.
    #[test]
    fn a_first_request_is_estimated_high_from_the_prompt_and_the_reserve() {
        let estimate = first_request_tokens(&"x".repeat(79_670), 8192);
        assert_eq!(estimate, 34_749);
        assert!(estimate >= 30_245);
        assert_eq!(first_request_tokens("", 8192), 8192);
    }

    /// 30 bytes are 10 tokens: with the reserve, exactly the budget.
    #[test]
    fn a_prompt_fits_up_to_its_budget_and_not_a_token_more() {
        let reviewer = limit(Role::Reviewer);
        assert_eq!(reviewer.over(&"x".repeat(30), None), None);
        assert!(reviewer.over(&"x".repeat(31), None).is_some());
    }

    #[test]
    fn a_prompt_over_its_budget_says_whose_it_is_and_what_to_change() {
        let task = TaskId::parse("T003").unwrap();
        assert_eq!(
            limit(Role::Reviewer).over(&"x".repeat(31), Some(&task)),
            Some("the reviewer prompt for T003, 31 bytes, is about 8203 tokens with the harness's reserve, over the reviewer's budget of 8202; mlx-serve/Qwen3.8-27B's window is 65536 tokens. To go on, lower params.diffShare or params.diffLimitBytes in .tenaz/run.json and run with --resume, or give the reviewer a model with a larger window in a new run (tenaz init --model-for, --context-window-for)".to_owned())
        );
        assert_eq!(
            limit(Role::Replan).over(&"x".repeat(31), None),
            Some("the replan prompt, 31 bytes, is about 8203 tokens with the harness's reserve, over the replan's budget of 8202; mlx-serve/Qwen3.8-27B's window is 65536 tokens. To go on, give the replan a model with a larger window in a new run (tenaz init --model-for, --context-window-for)".to_owned())
        );
    }

    #[test]
    fn giving_up_on_capacity_names_the_lowered_budget() {
        assert_eq!(
            give_up_reason("009-builder-T002", GiveUp::Capacity, Some(30_000)),
            "sessions still overflow the engine's memory after the context budget was lowered to 30000 (last: 009-builder-T002)"
        );
    }

    #[test]
    fn the_log_line_of_a_rerun_names_its_cause() {
        for (cause, reason) in [
            (
                FailureCause::OutOfMemory,
                "the engine ran out of memory; free memory (another program may be using the GPU) or reload the model",
            ),
            (FailureCause::Engine, "engine error"),
            (
                FailureCause::Harness,
                "the harness ended before the turn settled",
            ),
        ] {
            assert_eq!(retry_reason(cause), reason, "{cause:?}");
        }
    }

    #[test]
    fn the_first_rerun_notice_says_what_to_do_and_none_follows_a_harness_exit() {
        for (cause, notice) in [
            (
                FailureCause::OutOfMemory,
                Some(
                    "The engine ran out of memory: free memory (another program may be using the GPU) or reload the model. The run retries the same session.",
                ),
            ),
            (
                FailureCause::Engine,
                Some(
                    "The engine is failing (often memory). Free memory; the run retries the same session.",
                ),
            ),
            (FailureCause::Harness, None),
        ] {
            assert_eq!(retry_notice(cause).as_deref(), notice, "{cause:?}");
        }
    }
}
