//! The phases: bootstrap, the task loop (build, verify, review, split,
//! block, replan) and the final review, one module each. Here: what they
//! share, the `verify` call and the prompt.

use std::time::Duration;

use crate::briefing::{self, Briefing};
use crate::role::Role;
use crate::spec;
use crate::verify::{VerifyOutcome, interpret, retarget};

use super::{RunError, Runner};

mod bootstrap;
mod final_review;
mod planner;
mod review;
mod task;
mod task_loop;

/// What a session's prompt adds to the role prompt, spec and map.
#[derive(Default)]
struct PromptParts<'a> {
    /// Header lines, `Key: value`.
    fields: Vec<(String, String)>,
    /// The task card's path and text.
    card: Option<(&'a str, &'a str)>,
    /// Titled sections after the card.
    extra: Vec<(String, String)>,
    /// The requirements to keep from the spec (`spec::cited`); the whole
    /// spec when empty.
    cites: &'a [String],
}

impl Runner {
    pub(crate) async fn run_verify(&self, args: &[&str]) -> VerifyOutcome {
        let timeout = Duration::from_secs(self.params.verify_timeout_sec);
        let raw = self.deps.verify.run(args, timeout).await;
        interpret(
            args,
            &raw,
            self.params.verify_timeout_sec,
            self.params.verify_clip_bytes,
        )
    }

    /// Composes a prompt from the project's role prompt, spec and map, and
    /// what `parts` adds for this session.
    fn briefing(&self, role: Role, parts: PromptParts<'_>) -> Result<String, RunError> {
        let PromptParts {
            mut fields,
            card,
            extra,
            cites,
        } = parts;
        let root = self.project.root().display().to_string();
        if root.chars().any(char::is_whitespace) {
            // Models write unquoted paths; a space splits them in the shell.
            fields.push((
                "Project folder".to_owned(),
                format!(
                    "{root} (it contains spaces: quote it in shell commands, or use relative paths)"
                ),
            ));
        }
        let protocol = self.project.protocol()?;
        let mut role_prompt = self.project.role_prompt(role)?;
        if role == Role::Bootstrap {
            role_prompt =
                briefing::bootstrap_step(&role_prompt, self.ledger.bootstrap_step.number());
        }
        let spec = self
            .project
            .read_if(".tenaz/spec.md")
            .map(|spec| spec::cited(&spec, cites).unwrap_or(spec));
        let map = self.project.read_if(".tenaz/map.md");
        let b = Briefing {
            protocol: &protocol,
            role_prompt: &role_prompt,
            spec: spec.as_deref(),
            map: map.as_deref(),
            map_clip_bytes: self.params.map_clip_bytes,
            packed: self.params.briefing || role != Role::Builder,
            fields,
            card,
            extra,
        };
        Ok(retarget(
            &briefing::build(role, &b),
            self.params.verify_path(),
        ))
    }
}
