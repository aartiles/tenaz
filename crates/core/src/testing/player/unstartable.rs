//! The scripted harness's sessions that end before their first turn, as
//! Pi does without its agent directory (ADR 0037).

use super::Player;
use crate::session::SessionEvent;

/// What Pi 1.4.0 writes to stderr, before it exits 1, when its agent
/// directory does not list its model (here, the fake run's).
pub const PI_MODEL_NOT_FOUND: &str =
    r#"Error: Model "fake/fake" not found. Use --list-models to see available models."#;

impl Player {
    /// When the script's unstartable session names `task` and its count is
    /// not used up, ends before the first turn, as Pi does without its
    /// agent directory. True when it did.
    pub(super) fn unstartable(&mut self, task: &str) -> bool {
        let Some(script) = self.script.unstartable.clone() else {
            return false;
        };
        if script.task != task {
            return false;
        }
        let count = self.bump(&format!("unstartable-{task}"));
        if count > script.times {
            return false;
        }
        if let Some(log) = &self.stderr_log {
            let _ = std::fs::write(log, format!("{PI_MODEL_NOT_FOUND}\n"));
        }
        if let Some((at, down)) = &script.engine_down
            && *at == count
        {
            down.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.emit(SessionEvent::Exited { code: Some(1) });
        true
    }
}
