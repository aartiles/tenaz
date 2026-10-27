//! Readings a test gives in advance, for the memory probe and the engine's
//! counters.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::memory::MemoryReading;
use crate::metrics::EngineCounters;
use crate::ports::{EngineMetricsSource, MemoryProbe};

/// Answers with the values it was given, in order, and then the last one
/// again; with none, it has nothing to read.
#[derive(Debug)]
pub struct Replay<T> {
    values: Mutex<VecDeque<T>>,
}

impl<T: Clone> Replay<T> {
    pub fn new(values: impl IntoIterator<Item = T>) -> Self {
        Self {
            values: Mutex::new(values.into_iter().collect()),
        }
    }

    fn next(&self) -> Option<T> {
        let mut values = self.values.lock().ok()?;
        if values.len() > 1 {
            values.pop_front()
        } else {
            values.front().cloned()
        }
    }
}

#[async_trait]
impl MemoryProbe for Replay<MemoryReading> {
    async fn read(&self) -> Option<MemoryReading> {
        self.next()
    }
}

#[async_trait]
impl EngineMetricsSource for Replay<EngineCounters> {
    async fn counters(&self) -> Option<EngineCounters> {
        self.next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn values_come_in_order_and_the_last_one_stays() {
        let replay = Replay::new([
            MemoryReading::default(),
            MemoryReading {
                page_bytes: 4096,
                ..MemoryReading::default()
            },
        ]);
        assert_eq!(replay.read().await.map(|r| r.page_bytes), Some(0));
        assert_eq!(replay.read().await.map(|r| r.page_bytes), Some(4096));
        assert_eq!(replay.read().await.map(|r| r.page_bytes), Some(4096));
    }

    #[tokio::test]
    async fn with_no_values_there_is_nothing_to_read() {
        assert_eq!(Replay::<EngineCounters>::new([]).counters().await, None);
    }
}
