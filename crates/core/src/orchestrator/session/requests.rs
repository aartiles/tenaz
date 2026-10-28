//! The model responses of a session, recorded off its loop with the
//! engine's counters for each.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::metrics::{EngineCounters, RequestRecord, delta};
use crate::ports::{Clock, EngineMetricsSource};
use crate::project::Project;
use crate::session::{RequestSettings, ServerTimings, SessionEvent, StopReason, Usage};

/// One model response to record.
struct RequestInput {
    usage: Usage,
    stop_reason: StopReason,
    server: Option<ServerTimings>,
    sent: Option<RequestSettings>,
    timing: ClientTiming,
}

/// Client-side timings of one turn, in monotonic milliseconds.
#[derive(Clone, Copy, Debug)]
struct ClientTiming {
    turn_started: Option<u64>,
    first_token: Option<u64>,
    now: u64,
}

/// Records requests off the session loop: reading the engine's counters may
/// take a second per response, and the loop must keep up with the harness.
pub(super) struct RequestRecorder {
    tx: Option<UnboundedSender<RequestInput>>,
    task: JoinHandle<Vec<RequestRecord>>,
}

impl RequestRecorder {
    /// Starts recording the requests of session `label`, appended to
    /// `requests.jsonl` when there is a `project`, with the engine's
    /// counters read from `source`.
    pub(super) fn start(
        label: &str,
        project: Option<Project>,
        source: Option<Arc<dyn EngineMetricsSource>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RequestInput>();
        let label = label.to_owned();
        let task = tokio::spawn(async move {
            let mut engine = match source {
                Some(source) => Some(EngineWatch::start(source, clock).await),
                None => None,
            };
            let mut requests: Vec<RequestRecord> = Vec::new();
            while let Some(input) = rx.recv().await {
                let change = match &mut engine {
                    Some(engine) => engine.delta().await,
                    None => None,
                };
                let sequence = u32::try_from(requests.len() + 1).unwrap_or(u32::MAX);
                let record = request_record(&label, sequence, input).with_engine(change);
                if let Some(engine) = &mut engine {
                    engine.saw(record.engine_matched);
                }
                if let Some(project) = &project
                    && let Ok(line) = serde_json::to_string(&record)
                {
                    let _ = project.append(".tenaz/private/requests.jsonl", &format!("{line}\n"));
                }
                requests.push(record);
            }
            requests
        });
        Self { tx: Some(tx), task }
    }

    fn record(&self, input: RequestInput) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(input);
        }
    }

    /// Closes the queue and waits for every record.
    pub(super) async fn finish(mut self) -> Vec<RequestRecord> {
        self.tx.take();
        self.task.await.unwrap_or_default()
    }
}

/// The engine's counters as last read, to give each response its change.
struct EngineWatch {
    source: Arc<dyn EngineMetricsSource>,
    clock: Arc<dyn Clock>,
    before: Option<EngineCounters>,
    /// The engine was seen serving this session: a response's counters
    /// matched it.
    matched: bool,
}

impl EngineWatch {
    async fn start(source: Arc<dyn EngineMetricsSource>, clock: Arc<dyn Clock>) -> Self {
        let before = source.counters().await;
        Self {
            source,
            clock,
            before,
            matched: false,
        }
    }

    /// Notes whether a response's counters `matched` it.
    fn saw(&mut self, matched: bool) {
        self.matched |= matched;
    }

    /// The counters' change since the previous response. The engine may
    /// publish them just after the stream closes, so wait for them to move
    /// in steps of 100 ms: up to a second only once the engine has been
    /// seen serving this session; otherwise a foreign engine's counters
    /// would cost a second per response. `None` when they cannot be read.
    async fn delta(&mut self) -> Option<EngineCounters> {
        let waits = if self.matched { 10 } else { 2 };
        let prior = self.before.as_ref()?;
        let mut now = self.source.counters().await;
        for _ in 0..waits {
            match &now {
                Some(n) if n.get("requests") == prior.get("requests") => {
                    self.clock.sleep(Duration::from_millis(100)).await;
                    now = self.source.counters().await;
                }
                _ => break,
            }
        }
        let now = now?;
        let change = delta(&now, prior);
        self.before = Some(now);
        Some(change)
    }
}

fn seconds_between(from: u64, to: u64) -> f64 {
    f64::from(u32::try_from(to.saturating_sub(from)).unwrap_or(u32::MAX)) / 1000.0
}

/// The harness's view of one model response; the caller attaches the
/// engine's change.
fn request_record(label: &str, sequence: u32, input: RequestInput) -> RequestRecord {
    let RequestInput {
        usage,
        stop_reason,
        server,
        sent,
        timing,
    } = input;
    RequestRecord {
        session: label.to_owned(),
        sequence,
        stop_reason: Some(stop_reason.as_str().to_owned()),
        prompt_tokens: usage.input + usage.cache_read,
        cached_tokens: usage.cache_read,
        output_tokens: usage.output,
        reasoning_tokens: usage.reasoning,
        client_first_token_s: timing
            .turn_started
            .zip(timing.first_token)
            .map(|(t, f)| seconds_between(t, f)),
        client_total_s: timing.turn_started.map(|t| seconds_between(t, timing.now)),
        engine: None,
        engine_matched: false,
        server,
        sent,
    }
}

/// When the current turn started and gave its first token, for the
/// requests recorded at each message end.
#[derive(Default)]
pub(super) struct TurnTiming {
    turn_started: Option<u64>,
    first_token: Option<u64>,
}

impl TurnTiming {
    pub(super) fn observe(&mut self, event: &SessionEvent, now: u64, recorder: &RequestRecorder) {
        match event {
            SessionEvent::TurnStart => {
                self.turn_started = Some(now);
                self.first_token = None;
            }
            SessionEvent::FirstToken => {
                self.first_token.get_or_insert(now);
            }
            SessionEvent::MessageEnd {
                usage,
                stop_reason,
                server,
                sent,
                ..
            } => recorder.record(RequestInput {
                usage: *usage,
                stop_reason: stop_reason.clone(),
                server: *server,
                sent: sent.clone(),
                timing: ClientTiming {
                    turn_started: self.turn_started,
                    first_token: self.first_token,
                    now,
                },
            }),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FastClock, Replay};

    fn counters(requests: f64, tokens: f64) -> EngineCounters {
        EngineCounters::from([
            ("requests".to_owned(), requests),
            ("tokens".to_owned(), tokens),
        ])
    }

    /// A watch whose last reading was `before`, over the readings `source`
    /// gives next.
    fn watch(source: Replay<EngineCounters>, before: EngineCounters) -> EngineWatch {
        EngineWatch {
            source: Arc::new(source),
            clock: Arc::new(FastClock::new(100)),
            before: Some(before),
            matched: false,
        }
    }

    #[tokio::test]
    async fn the_engine_delta_waits_for_the_counters_to_move_and_no_longer() {
        let source = Replay::new([
            counters(3.0, 100.0),
            counters(3.0, 100.0),
            counters(4.0, 160.0),
            counters(5.0, 200.0),
        ]);
        let mut engine = watch(source, counters(3.0, 100.0));
        let change = engine.delta().await;
        assert_eq!(change, Some(counters(1.0, 60.0)));
        assert_eq!(engine.before, Some(counters(4.0, 160.0)));
    }

    #[tokio::test]
    async fn counters_that_never_move_give_a_zero_delta() {
        let source = Replay::new([counters(3.0, 100.0)]);
        let mut engine = watch(source, counters(3.0, 100.0));
        let change = engine.delta().await;
        assert_eq!(change, Some(counters(0.0, 0.0)));
        assert_eq!(engine.before, Some(counters(3.0, 100.0)));
    }

    #[tokio::test]
    async fn a_watch_with_no_first_reading_gives_no_delta() {
        let mut engine = watch(Replay::new([counters(4.0, 160.0)]), counters(0.0, 0.0));
        engine.before = None;
        assert_eq!(engine.delta().await, None);
        assert_eq!(engine.before, None);
    }

    /// Pi reports reasoning per message; the request row keeps it, and rows
    /// written before the field existed still read.
    #[test]
    fn request_rows_keep_reasoning_tokens() {
        let input = RequestInput {
            usage: Usage {
                input: 1871,
                output: 111,
                cache_read: 2048,
                reasoning: 57,
                ..Default::default()
            },
            stop_reason: StopReason::ToolUse,
            server: Some(ServerTimings {
                prompt_ms: Some(60.0),
                ..Default::default()
            }),
            sent: Some(RequestSettings {
                temperature: Some(0.6),
                ..Default::default()
            }),
            timing: ClientTiming {
                turn_started: Some(1_000),
                first_token: Some(4_424),
                now: 9_420,
            },
        };
        let row = request_record("001-bootstrap", 1, input);
        assert_eq!(
            (
                row.prompt_tokens,
                row.cached_tokens,
                row.output_tokens,
                row.reasoning_tokens
            ),
            (3919, 2048, 111, 57)
        );
        assert_eq!(row.client_first_token_s, Some(3.424));
        assert_eq!(row.server.and_then(|s| s.prompt_ms), Some(60.0));
        assert_eq!(row.sent.and_then(|s| s.temperature), Some(0.6));
        let old: RequestRecord = serde_json::from_str(
            r#"{"session":"s","sequence":1,"stop_reason":"stop","prompt_tokens":1,"cached_tokens":0,"output_tokens":1,"client_first_token_s":null,"client_total_s":null,"engine":null,"engine_matched":false}"#,
        )
        .unwrap();
        assert_eq!(old.reasoning_tokens, 0);
    }
}
