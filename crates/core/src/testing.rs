//! Fakes for tests: a scripted harness that exercises the orchestrator
//! without a model (a happy path, each recovery opt-in), a clock that runs
//! faster than real time, replayed memory readings and engine counters, the
//! scripted harness's `verify` judged without a shell, and a repository kept
//! as snapshots in memory with the contract that holds it to git.

use std::time::Duration;

use async_trait::async_trait;

mod harness;
mod notifier;
mod player;
mod replay;
mod vcs;
pub mod vcs_contract;
mod verify;

pub use harness::ScriptedHarness;
pub use notifier::SilentNotifier;
pub use player::VERIFY;
pub use replay::Replay;
pub use vcs::SnapshotVcs;
pub use vcs_contract::MakeVcs;
pub use verify::ScriptedVerify;

use crate::ports::Clock;

/// A clock that runs `speed` times faster than real time, so backoffs and
/// grace periods in tests take milliseconds. It reads and sleeps on tokio's
/// clock, so under paused time (`start_paused`) both stand still together.
#[derive(Debug)]
pub struct FastClock {
    started: tokio::time::Instant,
    speed: u32,
}

impl FastClock {
    pub fn new(speed: u32) -> Self {
        Self {
            started: tokio::time::Instant::now(),
            speed: speed.max(1),
        }
    }
}

#[async_trait]
impl Clock for FastClock {
    fn now_rfc3339(&self) -> String {
        let now = time::OffsetDateTime::now_utc();
        now.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default()
    }

    fn monotonic_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
            * u64::from(self.speed)
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration / self.speed).await;
    }
}
