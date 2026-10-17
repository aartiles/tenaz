//! The system clock.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use smr_core::ports::Clock;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Wall-clock time from the OS, monotonic time since construction.
#[derive(Clone, Debug)]
pub struct SystemClock {
    started: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

/// RFC 3339 with milliseconds, like JavaScript's `toISOString`.
pub fn rfc3339_millis(now: OffsetDateTime) -> String {
    let truncated = now
        .replace_nanosecond(u32::from(now.millisecond()) * 1_000_000)
        .unwrap_or(now);
    truncated
        .format(&Rfc3339)
        .unwrap_or_else(|_| now.to_string())
}

#[async_trait]
impl Clock for SystemClock {
    fn now_rfc3339(&self) -> String {
        rfc3339_millis(OffsetDateTime::now_utc())
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

    #[test]
    fn formats_like_to_iso_string() {
        let t = OffsetDateTime::from_unix_timestamp_nanos(1_760_000_000_123_456_789).unwrap();
        assert_eq!(rfc3339_millis(t), "2025-10-09T08:53:20.123Z");
    }

    #[tokio::test]
    async fn monotonic_time_advances() {
        let clock = SystemClock::new();
        let a = clock.monotonic_ms();
        clock.sleep(Duration::from_millis(5)).await;
        assert!(clock.monotonic_ms() >= a + 5);
        assert!(clock.now_rfc3339().ends_with('Z'));
    }
}
