//! The run's clock on tokio's time, for tests under paused time.

use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::ports::Clock;

/// A clock that reads and sleeps on tokio's clock at its own pace. A test
/// runs it with tokio's time paused (`#[tokio::test(start_paused = true)]`):
/// then a wait takes no real time, the clock jumps to the next timer only
/// when every task is idle, and timers fire in the order of their
/// deadlines however busy the machine is (ADR 0006).
///
/// Wall-clock time is the time it was made plus the time that has passed
/// on tokio's clock, so the journal's times agree with the run's waits.
#[derive(Debug)]
pub struct VirtualClock {
    started: tokio::time::Instant,
    wall: OffsetDateTime,
}

impl Default for VirtualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualClock {
    pub fn new() -> Self {
        Self {
            started: tokio::time::Instant::now(),
            wall: OffsetDateTime::now_utc(),
        }
    }
}

#[async_trait]
impl Clock for VirtualClock {
    fn now_rfc3339(&self) -> String {
        (self.wall + self.started.elapsed())
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default()
    }

    fn monotonic_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(rfc3339: &str) -> OffsetDateTime {
        OffsetDateTime::parse(rfc3339, &time::format_description::well_known::Rfc3339).unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn wall_time_moves_with_the_waits() {
        let clock = VirtualClock::new();
        let before = parse(&clock.now_rfc3339());
        clock.sleep(Duration::from_secs(3600)).await;
        assert_eq!(clock.monotonic_ms(), 3_600_000);
        let after = parse(&clock.now_rfc3339());
        assert_eq!(after - before, time::Duration::hours(1));
    }

    #[tokio::test(start_paused = true)]
    async fn timers_fire_in_the_order_of_their_deadlines() {
        let clock = std::sync::Arc::new(VirtualClock::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        for secs in [5, 1, 3] {
            let (clock, tx) = (clock.clone(), tx.clone());
            tokio::spawn(async move {
                clock.sleep(Duration::from_secs(secs)).await;
                let _ = tx.send(secs);
            });
        }
        drop(tx);
        let mut fired = vec![];
        while let Some(secs) = rx.recv().await {
            fired.push(secs);
        }
        assert_eq!(fired, [1, 3, 5]);
        assert_eq!(clock.monotonic_ms(), 5_000);
    }
}
