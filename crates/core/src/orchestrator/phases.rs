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

    /// Composes a prompt from the project's role prompt, spec and map.
    fn briefing(
        &self,
        role: Role,
        fields: Vec<(String, String)>,
        card: Option<(&str, &str)>,
        extra: Vec<(String, String)>,
        cites: &[String],
    ) -> Result<String, RunError> {
        let mut fields = fields;
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
