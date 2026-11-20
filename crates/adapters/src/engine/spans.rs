//! When Tenaz's own engine commands ran (ADR 0039).

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use tenaz_core::leftover::Span;

/// How long ago `time` was by the wall clock; zero for a time ahead of it
/// (the clock was set back).
pub(crate) fn ago(time: SystemTime) -> Duration {
    SystemTime::now().duration_since(time).unwrap_or_default()
}

/// When Tenaz's own engine commands ran, each with its wait for the
/// model. The engine an `--engine-start` script starts may detach from
/// the project in the middle of an agent session, as what the agent
/// leaves does; what was born while one of these ran is spared. Clones
/// share the record. Wall-clock times, as `ps` ages are (see
/// [`crate::process`]).
#[derive(Clone, Debug, Default)]
pub struct EngineSpans(Arc<Mutex<Vec<Ran>>>);

/// When one engine command began, and ended once it has.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ran {
    pub(crate) began: SystemTime,
    pub(crate) ended: Option<SystemTime>,
}

impl EngineSpans {
    /// Notes that an engine command begins now; it ends when the guard
    /// is dropped.
    pub fn begin(&self) -> EngineSpan {
        let mut spans = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        spans.push(Ran {
            began: SystemTime::now(),
            ended: None,
        });
        EngineSpan {
            spans: self.clone(),
            index: spans.len() - 1,
        }
    }

    /// Notes a command that ran from `began` to `ended`.
    #[cfg(test)]
    pub(crate) fn noted(&self, began: SystemTime, ended: SystemTime) {
        let mut spans = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        spans.push(Ran {
            began,
            ended: Some(ended),
        });
    }

    /// Each span as how long ago it began and ended; one still running
    /// ends now.
    pub(crate) fn ages(&self) -> Vec<Span> {
        self.ran()
            .iter()
            .map(|ran| Span {
                began: ago(ran.began),
                ended: ran.ended.map_or(Duration::ZERO, ago),
            })
            .collect()
    }

    /// Each command's times, in the order they began.
    pub(crate) fn ran(&self) -> Vec<Ran> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// An engine command running; dropped, it has ended.
#[derive(Debug)]
pub struct EngineSpan {
    spans: EngineSpans,
    index: usize,
}

impl Drop for EngineSpan {
    fn drop(&mut self) {
        let mut spans = self.spans.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(span) = spans.get_mut(self.index) {
            span.ended = Some(SystemTime::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command still running ends now: what it starts until the sweep
    /// is spared.
    #[test]
    fn a_span_still_running_ends_now() {
        let spans = EngineSpans::default();
        let _running = spans.begin();
        let ages = spans.ages();
        assert_eq!(ages.len(), 1);
        assert_eq!(ages[0].ended, Duration::ZERO);
        assert!(ages[0].began < Duration::from_secs(1), "{ages:?}");
        assert!(spans.ran()[0].ended.is_none());
    }
}
