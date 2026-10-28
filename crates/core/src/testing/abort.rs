//! Whether the scripted session's turn was aborted.

use tokio::sync::watch;

/// Whether the session's turn was aborted. A call that runs until the
/// abort waits on it, so it costs nothing however far the paused clock
/// jumps (ADR 0006).
#[derive(Debug, Default)]
pub(super) struct Abort(watch::Sender<bool>);

impl Abort {
    pub(super) fn set(&self) {
        self.0.send_replace(true);
    }

    pub(super) fn clear(&self) {
        self.0.send_replace(false);
    }

    pub(super) fn is_set(&self) -> bool {
        *self.0.borrow()
    }

    /// Returns once the turn is aborted.
    pub(super) async fn wait(&self) {
        let _ = self.0.subscribe().wait_for(|aborted| *aborted).await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Whether a wait for the abort returns within a minute of paused time.
    async fn returns(abort: &Abort) -> bool {
        tokio::time::timeout(Duration::from_secs(60), abort.wait())
            .await
            .is_ok()
    }

    #[tokio::test(start_paused = true)]
    async fn a_wait_for_the_abort_returns_once_it_is_set() {
        let abort = Abort::default();
        assert!(!returns(&abort).await);
        abort.set();
        assert!(returns(&abort).await);
    }

    #[tokio::test(start_paused = true)]
    async fn a_cleared_abort_holds_the_next_wait() {
        let abort = Abort::default();
        abort.set();
        abort.clear();
        assert!(!returns(&abort).await);
    }
}
