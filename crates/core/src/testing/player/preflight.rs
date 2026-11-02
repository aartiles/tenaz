//! The scripted harness's answers to the preflight's prompts.

use super::Player;
use crate::orchestrator::TOOL_PROBE_FILE;

impl Player {
    /// Answers a prompt without a role, as the preflight sends: an engine
    /// error when the script refuses the preflight, else the word the tool
    /// probe printed, or the product the other prompts ask for.
    pub(super) fn preflight(&self, prompt: &str) {
        if let Some(message) = &self.script.refused_preflight {
            return self.engine_error(message);
        }
        if prompt.contains(TOOL_PROBE_FILE) {
            let printed = std::fs::read_to_string(self.cwd.join(TOOL_PROBE_FILE));
            return self.finish(printed.unwrap_or_default().trim());
        }
        self.finish("323");
    }
}
