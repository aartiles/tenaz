//! How a scripted session keeps step with the runner without polling: a
//! poll would move tokio's paused clock on every pass (ADR 0006).

use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::watch;

/// How far the runner has read a session's events: it asks for the next
/// one only after handling the last. An ask the runner gave up on (its
/// wait lost to a timer) is asked again for the same event, not counted
/// twice.
#[derive(Debug, Default)]
pub(super) struct Pace {
    sent: AtomicU64,
    received: AtomicU64,
    /// How many events the runner has asked for, the one it waits on
    /// included.
    asked: watch::Sender<u64>,
}

impl Pace {
    /// The session sent an event.
    pub(super) fn sent(&self) {
        self.sent.fetch_add(1, Ordering::SeqCst);
    }

    /// The runner asks for the next event, having handled every one it
    /// received.
    pub(super) fn ask(&self) {
        self.asked
            .send_replace(self.received.load(Ordering::SeqCst) + 1);
    }

    /// The runner received an event.
    pub(super) fn received(&self) {
        self.received.fetch_add(1, Ordering::SeqCst);
    }

    /// The runner reads no more: nothing waits on it any longer.
    pub(super) fn close(&self) {
        self.asked.send_replace(u64::MAX);
    }

    /// Returns once the runner has handled every event sent so far, so a
    /// steer or abort it sends in reply has arrived.
    pub(super) async fn handled(&self) {
        let sent = self.sent.load(Ordering::SeqCst);
        let _ = self.asked.subscribe().wait_for(|asked| *asked > sent).await;
    }
}

/// Whether the session's turn was aborted. A call that runs until the
/// abort waits on it, so it costs nothing however far the paused clock
/// jumps.
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
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    /// Whether `handled` returns within a minute of paused time.
    async fn released(pace: &Pace) -> bool {
        tokio::time::timeout(Duration::from_secs(60), pace.handled())
            .await
            .is_ok()
    }

    #[tokio::test(start_paused = true)]
    async fn an_event_is_handled_when_the_runner_asks_for_the_next() {
        let pace = Pace::default();
        pace.sent();
        pace.ask();
        assert!(!released(&pace).await, "the event was only asked for");
        pace.received();
        assert!(!released(&pace).await, "the event was only received");
        pace.ask();
        assert!(released(&pace).await);
    }

    #[tokio::test(start_paused = true)]
    async fn an_ask_given_up_and_asked_again_is_not_a_handled_event() {
        let pace = Pace::default();
        pace.sent();
        pace.ask();
        pace.ask();
        assert!(!released(&pace).await);
    }

    #[tokio::test(start_paused = true)]
    async fn closing_releases_a_wait_for_the_runner() {
        let pace = Pace::default();
        pace.sent();
        pace.close();
        assert!(released(&pace).await);
    }

    #[tokio::test(start_paused = true)]
    async fn a_wait_for_the_abort_returns_once_it_is_set() {
        let abort = Arc::new(Abort::default());
        let waiting = tokio::spawn({
            let abort = abort.clone();
            async move { abort.wait().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        abort.set();
        waiting.await.unwrap();
        abort.clear();
        assert!(!abort.is_set());
    }
}
