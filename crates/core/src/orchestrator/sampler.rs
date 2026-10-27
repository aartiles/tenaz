//! Periodic memory sampling to `.tenaz/private/memory.jsonl`, with a window
//! per session for the journal.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::memory::{MemorySample, MemoryWindow};
use crate::ports::{Clock, MemoryProbe};
use crate::project::Project;

#[derive(Default)]
struct State {
    previous: Option<MemorySample>,
    window: Option<MemoryWindow>,
    label: Option<String>,
}

/// Samples on a timer and at the start and end of every session.
#[derive(Clone)]
pub struct Sampler {
    probe: Arc<dyn MemoryProbe>,
    clock: Arc<dyn Clock>,
    project: Project,
    interval: Duration,
    state: Arc<Mutex<State>>,
    cancel: CancellationToken,
}

impl std::fmt::Debug for Sampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sampler")
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

impl Sampler {
    pub fn new(
        probe: Arc<dyn MemoryProbe>,
        clock: Arc<dyn Clock>,
        project: Project,
        interval: Duration,
    ) -> Self {
        Self {
            probe,
            clock,
            project,
            interval,
            state: Arc::new(Mutex::new(State::default())),
            cancel: CancellationToken::new(),
        }
    }

    /// Takes one sample, appends it, and folds it into the open window.
    pub async fn tick(&self) {
        let Some(reading) = self.probe.read().await else {
            return;
        };
        let mut state = self.state.lock().await;
        let sample = reading.into_sample(
            self.clock.now_rfc3339(),
            state.label.clone(),
            state.previous.as_ref(),
        );
        if let Ok(line) = serde_json::to_string(&sample) {
            let _ = self
                .project
                .append(".tenaz/private/memory.jsonl", &format!("{line}\n"));
        }
        if let Some(window) = &mut state.window {
            window.add(&sample);
        }
        state.previous = Some(sample);
    }

    /// Starts the timer task.
    pub fn start(&self) {
        let sampler = self.clone();
        tokio::spawn(async move {
            loop {
                sampler.tick().await;
                tokio::select! {
                    () = sampler.cancel.cancelled() => return,
                    () = sampler.clock.sleep(sampler.interval) => {}
                }
            }
        });
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }

    /// Opens a window for the session `label`, sampling once first.
    pub async fn begin(&self, label: &str) {
        self.state.lock().await.label = Some(label.to_owned());
        self.tick().await;
        self.state.lock().await.window = Some(MemoryWindow::default());
    }

    /// Samples once more and closes the window.
    pub async fn end(&self) -> Option<MemoryWindow> {
        self.tick().await;
        let mut state = self.state.lock().await;
        state.label = None;
        state.window.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryReading;
    use crate::testing::{FastClock, Replay};

    fn reading(swapins: u64, server_mem_gib: f64) -> MemoryReading {
        MemoryReading {
            swapins: Some(swapins),
            server_mem_gib: Some(server_mem_gib),
            page_bytes: 16_384,
            ..MemoryReading::default()
        }
    }

    #[tokio::test]
    async fn a_session_window_holds_what_happened_during_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::new(dir.path());
        let sampler = Sampler::new(
            Arc::new(Replay::new([reading(10, 20.0), reading(14, 30.0)])),
            Arc::new(FastClock::new(100)),
            project.clone(),
            Duration::from_secs(30),
        );
        sampler.begin("001-bootstrap").await;
        let window = sampler.end().await.unwrap();
        assert_eq!(window.samples, 1);
        assert_eq!(window.swapins, 4);
        assert!((window.server_mem_max_gib - 30.0).abs() < f64::EPSILON);
        let log = project.read(".tenaz/private/memory.jsonl").unwrap();
        assert_eq!(log.lines().count(), 2);
        assert!(log.lines().all(|l| l.contains("001-bootstrap")), "{log}");
        assert_eq!(sampler.end().await, None, "a second end has no window");
    }
}
