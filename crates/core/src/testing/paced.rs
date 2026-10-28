//! The scripted session's events, on a channel that knows how far the
//! runner has read them, so a turn can wait on the runner instead of
//! polling: a poll would move tokio's paused clock on every pass (ADR 0006).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{mpsc, watch};

use crate::session::SessionEvent;

/// What the runner asked for after it closed the session: more than any
/// count of events, so every wait on it returns.
const CLOSED: u64 = u64::MAX;

/// A channel of session events that counts what is sent and what the
/// runner has handled.
pub(super) fn channel() -> (PacedSender, PacedReceiver) {
    let (tx, rx) = mpsc::unbounded_channel();
    let pace = Arc::new(Pace::default());
    (
        PacedSender {
            tx,
            pace: pace.clone(),
        },
        PacedReceiver {
            rx,
            pace,
            received: 0,
        },
    )
}

/// How far the runner has read. It asks for the next event only after
/// handling the last; an ask it gave up on (its wait lost to a timer) is
/// asked again for the same event, not counted twice.
#[derive(Debug, Default)]
struct Pace {
    sent: AtomicU64,
    /// How many events the runner has asked for, the one it waits on
    /// included; [`CLOSED`] once it reads no more.
    asked: watch::Sender<u64>,
}

/// The scripted model's end: sends events and waits on the runner.
#[derive(Clone, Debug)]
pub(super) struct PacedSender {
    tx: mpsc::UnboundedSender<SessionEvent>,
    pace: Arc<Pace>,
}

impl PacedSender {
    pub(super) fn send(&self, event: SessionEvent) {
        if self.tx.send(event).is_ok() {
            self.pace.sent.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Returns once the runner has handled every event sent so far, so a
    /// steer or abort it sends in reply has arrived, or once it closed.
    pub(super) async fn handled(&self) {
        let sent = self.pace.sent.load(Ordering::SeqCst);
        self.wait_for(|asked| asked > sent).await;
    }

    /// Whether the runner has closed the session.
    pub(super) fn is_closed(&self) -> bool {
        *self.pace.asked.borrow() == CLOSED
    }

    /// Returns once the runner has closed the session.
    pub(super) async fn closed(&self) {
        self.wait_for(|asked| asked == CLOSED).await;
    }

    async fn wait_for(&self, done: impl Fn(u64) -> bool) {
        let _ = self.pace.asked.subscribe().wait_for(|a| done(*a)).await;
    }
}

/// The runner's end.
#[derive(Debug)]
pub(super) struct PacedReceiver {
    rx: mpsc::UnboundedReceiver<SessionEvent>,
    pace: Arc<Pace>,
    received: u64,
}

impl PacedReceiver {
    /// The next event, the runner having handled every one before it.
    pub(super) async fn recv(&mut self) -> Option<SessionEvent> {
        let next = self.received + 1;
        // Once closed, the channel stays closed.
        self.pace.asked.send_if_modified(|asked| {
            let open = *asked != CLOSED;
            if open {
                *asked = next;
            }
            open
        });
        let event = self.rx.recv().await?;
        self.received += 1;
        Some(event)
    }

    /// The runner reads no more: every wait on it returns.
    pub(super) fn close(&self) {
        self.pace.asked.send_replace(CLOSED);
    }
}

/// A session dropped without being closed reads no more either.
impl Drop for PacedReceiver {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Whether `wait` returns within a minute of paused time.
    async fn returns(wait: impl Future<Output = ()>) -> bool {
        tokio::time::timeout(Duration::from_secs(60), wait)
            .await
            .is_ok()
    }

    #[tokio::test(start_paused = true)]
    async fn an_event_is_handled_when_the_runner_asks_for_the_next() {
        let (tx, mut rx) = channel();
        tx.send(SessionEvent::TurnStart);
        assert!(!returns(tx.handled()).await, "nothing was read yet");
        assert!(rx.recv().await.is_some());
        assert!(!returns(tx.handled()).await, "the event was only received");
        assert!(!returns(async { drop(rx.recv().await) }).await);
        assert!(returns(tx.handled()).await);
    }

    #[tokio::test(start_paused = true)]
    async fn an_ask_given_up_and_asked_again_is_not_a_handled_event() {
        let (tx, mut rx) = channel();
        tx.send(SessionEvent::TurnStart);
        assert!(rx.recv().await.is_some());
        let gives_up = "the runner asks for the next event and gives up";
        assert!(
            !returns(async { drop(rx.recv().await) }).await,
            "{gives_up}"
        );
        assert!(
            !returns(async { drop(rx.recv().await) }).await,
            "{gives_up}"
        );
        tx.send(SessionEvent::TurnEnd);
        assert!(
            !returns(tx.handled()).await,
            "the second event was never read"
        );
        assert!(rx.recv().await.is_some());
        assert!(
            !returns(async { drop(rx.recv().await) }).await,
            "{gives_up}"
        );
        assert!(returns(tx.handled()).await, "read, and the next asked for");
    }

    #[tokio::test(start_paused = true)]
    async fn closing_releases_the_waits_on_the_runner_and_stays_closed() {
        let (tx, mut rx) = channel();
        tx.send(SessionEvent::TurnStart);
        let handled = tokio::spawn({
            let tx = tx.clone();
            async move { tx.handled().await }
        });
        let closed = tokio::spawn({
            let tx = tx.clone();
            async move { tx.closed().await }
        });
        assert!(!returns(tx.closed()).await);
        rx.close();
        assert!(returns(async { handled.await.unwrap() }).await);
        assert!(returns(async { closed.await.unwrap() }).await);
        assert!(
            rx.recv().await.is_some(),
            "an event sent before the close is still read"
        );
        assert!(tx.is_closed());
        assert!(returns(tx.handled()).await);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_runners_end_closes_it() {
        let (tx, rx) = channel();
        drop(rx);
        assert!(tx.is_closed());
    }
}
