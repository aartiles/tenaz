//! The lease: one resident engine. The LLM holds it by default; a media job
//! borrows it (unload LLM, load media, run, unload media, reload LLM), and
//! so does a heavy job a builder declared (ADR 0010: unload LLM, run,
//! reload LLM). The proxy's gate holds LLM requests while the LLM is away.
//! An engine that cannot unload keeps the LLM loaded, and the job runs next
//! to it. A run that swaps its models (ADR 0013) keeps one of them loaded
//! at a time, the resident one; `residency.rs` holds what depends on it.

#[cfg(test)]
mod fakes;
mod residency;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use thiserror::Error;
use tokio::sync::{Mutex, watch};

use crate::ports::{EngineAdapter, EngineError, MediaAdapter, MediaError, MediaJob, MediaOutput};
use residency::Residency;

/// Open or closed for LLM requests. The proxy awaits `wait_open` before
/// forwarding a request, so the harness never sees a refused connection.
#[derive(Clone, Debug)]
pub struct Gate {
    tx: watch::Sender<bool>,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    pub fn new() -> Self {
        Self {
            tx: watch::Sender::new(true),
        }
    }

    pub fn is_open(&self) -> bool {
        *self.tx.borrow()
    }

    pub fn close(&self) {
        self.tx.send_replace(false);
    }

    pub fn open(&self) {
        self.tx.send_replace(true);
    }

    /// Resolves at once when open, otherwise when the gate opens.
    pub async fn wait_open(&self) {
        let mut rx = self.tx.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Who holds the lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Holder {
    Llm,
    Media(String),
    /// A heavy job a builder declared (ADR 0010).
    Job,
}

impl std::fmt::Display for Holder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Holder::Llm => f.write_str("llm"),
            Holder::Media(name) => write!(f, "media:{name}"),
            Holder::Job => f.write_str("job"),
        }
    }
}

/// One step of a lease cycle, with its timing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseTransition {
    pub holder: Holder,
    /// `unload`, `load`, `job`, `ready`, `kept`, `failed`, `evicted`.
    pub transition: String,
    pub ms: u64,
    pub detail: Option<String>,
}

#[derive(Debug, Error)]
pub enum LeaseError {
    #[error("no media engine named {0}")]
    UnknownMedia(String),
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error(transparent)]
    Media(#[from] MediaError),
    #[error("{0} is not one of the run's models")]
    UnknownLlm(String),
    /// The job ran, but the LLMs could not be reloaded afterwards.
    #[error("the LLMs did not reload after a job: {0}")]
    ReloadFailed(EngineError),
}

/// What [`LeaseManager::run_job`] gives back: the job's output, unless it
/// never ran, and how the LLM fared around it. A job that ran keeps its
/// output even when the LLM did not reload after it.
#[derive(Debug)]
pub struct JobLease<T> {
    pub output: Option<T>,
    pub lease: Result<(), LeaseError>,
}

/// Observes transitions (the orchestrator journals them).
pub type Observer = Arc<dyn Fn(LeaseTransition) + Send + Sync>;

/// What the holder lock guards: who holds the lease, and which LLMs it
/// keeps loaded.
#[derive(Debug)]
struct LeaseState {
    holder: Holder,
    residency: Residency,
}

/// A lease step to time and journal: who takes it, what it is, and the
/// model it is about, which the record names when the step works.
struct Step<'a> {
    holder: Holder,
    transition: &'a str,
    about: Option<&'a str>,
}

impl<'a> Step<'a> {
    fn new(holder: Holder, transition: &'a str) -> Self {
        Self {
            holder,
            transition,
            about: None,
        }
    }

    fn llm(transition: &'a str, about: Option<&'a str>) -> Self {
        Self {
            about,
            ..Self::new(Holder::Llm, transition)
        }
    }
}

/// The lease manager.
pub struct LeaseManager {
    llm: Arc<dyn EngineAdapter>,
    /// The run's model first, then the other roles' models (ADR 0011).
    llm_models: Vec<String>,
    /// One model loaded at a time (ADR 0013); fixed by `with_swap`. It
    /// mirrors `Residency::One` in `state` so that `swaps` needs no lock.
    swap: bool,
    media: BTreeMap<String, Arc<dyn MediaAdapter>>,
    gate: Gate,
    state: Mutex<LeaseState>,
    observer: Observer,
}

impl std::fmt::Debug for LeaseManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseManager")
            .field("llm", &self.llm.name())
            .field("llm_models", &self.llm_models)
            .field("media", &self.media.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl LeaseManager {
    /// A lease over `llm_models`, the LLMs on `llm` the sessions play,
    /// all resident unless the lease swaps them (`with_swap`).
    pub fn new(
        llm: Arc<dyn EngineAdapter>,
        llm_models: Vec<String>,
        gate: Gate,
        observer: Observer,
    ) -> Self {
        Self {
            llm,
            llm_models,
            swap: false,
            media: BTreeMap::new(),
            gate,
            state: Mutex::new(LeaseState {
                holder: Holder::Llm,
                residency: Residency::All,
            }),
            observer,
        }
    }

    #[must_use]
    pub fn with_media(mut self, adapter: Arc<dyn MediaAdapter>) -> Self {
        self.media.insert(adapter.name().to_owned(), adapter);
        self
    }

    pub fn gate(&self) -> &Gate {
        &self.gate
    }

    pub fn media_names(&self) -> Vec<String> {
        self.media.keys().cloned().collect()
    }

    /// The current holder (waits if a cycle is in progress).
    pub async fn holder(&self) -> Holder {
        self.state.lock().await.holder.clone()
    }

    /// Brings back the LLMs the lease keeps loaded: those the engine does
    /// not list as ready, or, when swapping, the resident model alone.
    /// Used after an engine error, a job, or a failed reload. A failure
    /// leaves the gate as it was, closed after a failed reload or a swap
    /// that changed the engine.
    /// An engine that does not answer is an error here, not a reason to
    /// start it.
    pub async fn ensure_llm(&self) -> Result<(), LeaseError> {
        let mut state = self.state.lock().await;
        self.recover(&mut state.residency).await?;
        state.holder = Holder::Llm;
        self.gate.open();
        Ok(())
    }

    /// The whole cycle for one media job. The holder lock serialises cycles.
    pub async fn run_media(&self, name: &str, job: MediaJob) -> Result<MediaOutput, LeaseError> {
        let adapter = self
            .media
            .get(name)
            .cloned()
            .ok_or_else(|| LeaseError::UnknownMedia(name.to_owned()))?;
        let mut state = self.state.lock().await;
        let media_holder = Holder::Media(name.to_owned());
        let kept = if adapter.shares_engine() {
            // The engine renders the job itself and manages its memory.
            Some("the LLM's engine serves the media model too")
        } else if self.llm.can_unload() {
            None
        } else {
            // The media engine loads next to the LLM, so the memory has to hold both.
            Some("the engine cannot unload the LLM")
        };
        if let Some(why) = kept {
            self.observe(Holder::Llm, "kept", 0, Some(why.to_owned()));
            return self.media_cycle(&media_holder, adapter.as_ref(), job).await;
        }
        let (output, reloaded) = self
            .with_llm_away(
                &mut state,
                media_holder.clone(),
                self.media_cycle(&media_holder, adapter.as_ref(), job),
            )
            .await?;
        reloaded?;
        output
    }

    /// Runs `job` with the engine's memory to itself: LLM requests wait at
    /// the gate and the LLM is unloaded where the engine can, then loaded
    /// again. An engine that cannot unload keeps the LLM, and the job runs
    /// next to it with the gate closed. Stop a job through its own means (a
    /// cancellation token): dropped before it ends, the gate stays closed.
    pub async fn run_job<T>(&self, job: impl Future<Output = T>) -> JobLease<T> {
        let mut state = self.state.lock().await;
        let job = self.timed_ok(Holder::Job, "job", job);
        if !self.llm.can_unload() {
            self.observe(
                Holder::Llm,
                "kept",
                0,
                Some("the engine cannot unload the LLM".to_owned()),
            );
            self.gate.close();
            let output = job.await;
            self.gate.open();
            return JobLease {
                output: Some(output),
                lease: Ok(()),
            };
        }
        match self.with_llm_away(&mut state, Holder::Job, job).await {
            Ok((output, reloaded)) => JobLease {
                output: Some(output),
                lease: reloaded,
            },
            Err(error) => JobLease {
                output: None,
                lease: Err(error),
            },
        }
    }

    /// Frees the LLMs with their requests held at the gate, runs `work`
    /// as `borrower`, and loads the LLMs again. An error when freeing fails
    /// (`work` did not run, the gate is open again); otherwise the output
    /// and whether the reload worked. After a failed reload the gate stays
    /// closed: requests wait until `ensure_llm` succeeds.
    async fn with_llm_away<T>(
        &self,
        state: &mut LeaseState,
        borrower: Holder,
        work: impl Future<Output = T>,
    ) -> Result<(T, Result<(), LeaseError>), LeaseError> {
        self.gate.close();
        if let Err(error) = self.free(&mut state.residency).await {
            self.undo_free(&mut state.residency).await;
            self.gate.open();
            return Err(error.into());
        }
        state.holder = borrower;
        let output = work.await;
        let reloaded = match self.restore(&mut state.residency).await {
            Ok(()) => {
                state.holder = Holder::Llm;
                self.gate.open();
                Ok(())
            }
            Err(error) => {
                self.observe(Holder::Llm, "failed", 0, Some(error.to_string()));
                Err(LeaseError::ReloadFailed(error))
            }
        };
        Ok((output, reloaded))
    }

    /// `op`, which cannot fail, observed as a transition with its duration.
    async fn timed_ok<T>(
        &self,
        holder: Holder,
        transition: &str,
        op: impl Future<Output = T>,
    ) -> T {
        let started = Instant::now();
        let output = op.await;
        let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.observe(holder, transition, ms, None);
        output
    }

    async fn media_cycle(
        &self,
        holder: &Holder,
        adapter: &dyn MediaAdapter,
        job: MediaJob,
    ) -> Result<MediaOutput, LeaseError> {
        let step = |transition| Step::new(holder.clone(), transition);
        self.timed(step("load"), adapter.load()).await?;
        let output = self.timed(step("job"), adapter.run(job)).await;
        // Always free the media engine, even after a failed job.
        let unloaded = self.timed(step("unload"), adapter.unload()).await;
        let output = output?;
        unloaded?;
        Ok(output)
    }

    /// `op` observed as `step` with its duration, or as `failed` with the
    /// step and the error.
    async fn timed<T, E: std::fmt::Display>(
        &self,
        step: Step<'_>,
        op: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        let started = Instant::now();
        let result = op.await;
        let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let Step {
            holder,
            transition,
            about,
        } = step;
        match &result {
            Ok(_) => self.observe(holder, transition, ms, about.map(str::to_owned)),
            Err(error) => {
                self.observe(holder, "failed", ms, Some(format!("{transition}: {error}")));
            }
        }
        result
    }

    fn observe(&self, holder: Holder, transition: &str, ms: u64, detail: Option<String>) {
        (self.observer)(LeaseTransition {
            holder,
            transition: transition.to_owned(),
            ms,
            detail,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::fakes::{
        FakeLlm, Unload, job, kinds, noting_job, records, setup, setup_engine, setup_llm,
        setup_media, setup_two, two,
    };
    use super::*;

    /// Whether the LLMs hold the lease with the gate open.
    async fn llms_hold(manager: &LeaseManager) -> bool {
        manager.gate().is_open() && manager.holder().await == Holder::Llm
    }

    #[tokio::test]
    async fn a_media_job_cycles_the_engines_and_closes_the_gate_meanwhile() {
        let (manager, log, transitions) = setup(false, false);
        let output = manager.run_media("comfyui", job()).await.unwrap();
        assert_eq!(
            output.paths,
            vec![std::path::PathBuf::from("/tmp/out/out.png")]
        );
        assert_eq!(
            log.lines(),
            vec![
                "llm unload m",
                "comfy load",
                "comfy job image (gate closed)",
                "comfy unload",
                "llm load m"
            ]
        );
        assert!(manager.gate().is_open());
        assert_eq!(manager.holder().await, Holder::Llm);
        let kinds: Vec<String> = transitions
            .lock()
            .unwrap()
            .iter()
            .map(|t| format!("{} {}", t.holder, t.transition))
            .collect();
        assert_eq!(
            kinds,
            vec![
                "llm unload",
                "media:comfyui load",
                "media:comfyui job",
                "media:comfyui unload",
                "llm load",
                "llm ready"
            ]
        );
    }
    /// A heavy job (ADR 0010) gets the engine's memory: the LLM is unloaded
    /// and requests wait while it runs, then the LLM comes back.
    #[tokio::test]
    async fn a_heavy_job_runs_with_the_llm_unloaded_and_the_gate_closed() {
        let (manager, log, _) = setup(false, false);
        let ran = manager.run_job(noting_job(&manager, &log)).await;
        assert!(ran.lease.is_ok(), "{:?}", ran.lease);
        let after = (ran.output, manager.gate().is_open(), manager.holder().await);
        assert_eq!(after, (Some(7), true, Holder::Llm));
        assert_eq!(
            log.lines(),
            ["llm unload m", "job (gate open: false)", "llm load m"]
        );
    }

    /// An engine that cannot unload keeps the LLM; the job still runs with
    /// requests held at the gate.
    #[tokio::test]
    async fn a_heavy_job_next_to_an_llm_that_cannot_unload_still_holds_requests() {
        let (manager, log, transitions) = setup_llm(false, false, Unload::Unsupported);
        let ran = manager.run_job(noting_job(&manager, &log)).await;
        assert_eq!(
            (ran.lease.is_ok(), ran.output, manager.gate().is_open()),
            (true, Some(7), true)
        );
        assert_eq!(log.lines(), ["job (gate open: false)"]);
        assert!(
            transitions
                .lock()
                .unwrap()
                .iter()
                .any(|t| t.holder == Holder::Llm && t.transition == "kept")
        );
    }

    /// The job ran and its output stands; the LLM did not come back, so
    /// requests keep waiting at the gate until `ensure_llm` succeeds.
    #[tokio::test]
    async fn a_heavy_job_whose_reload_fails_keeps_its_output_and_the_gate_closed() {
        let (manager, log, _) = setup(false, true);
        let ran = manager.run_job(noting_job(&manager, &log)).await;
        assert_eq!(ran.output, Some(7));
        assert!(
            matches!(ran.lease, Err(LeaseError::ReloadFailed(_))),
            "{:?}",
            ran.lease
        );
        assert!(!manager.gate().is_open());
    }

    /// An LLM that would not unload may still be serving: the job does not
    /// run and requests go on.
    #[tokio::test]
    async fn a_heavy_job_whose_unload_fails_does_not_run() {
        let (manager, log, _) = setup_llm(false, false, Unload::Fails);
        let ran = manager.run_job(noting_job(&manager, &log)).await;
        let after = (ran.output, manager.gate().is_open(), log.lines());
        assert_eq!(after, (None, true, Vec::<String>::new()));
        assert!(
            matches!(ran.lease, Err(LeaseError::Engine(_))),
            "{:?}",
            ran.lease
        );
    }

    #[tokio::test]
    async fn a_failed_job_still_frees_the_media_engine_and_reloads_the_llm() {
        let (manager, log, transitions) = setup(true, false);
        let error = manager.run_media("comfyui", job()).await.unwrap_err();
        assert!(matches!(error, LeaseError::Media(MediaError::Failed(_))));
        assert_eq!(
            log.lines(),
            vec![
                "llm unload m",
                "comfy load",
                "comfy job image (gate closed)",
                "comfy unload",
                "llm load m"
            ]
        );
        assert!(manager.gate().is_open());
        assert!(
            transitions
                .lock()
                .unwrap()
                .iter()
                .any(|t| t.transition == "failed"
                    && t.detail.as_deref() == Some("job: media job failed: bad workflow"))
        );
    }

    #[tokio::test]
    async fn a_failed_reload_keeps_the_gate_closed_until_ensure_llm_succeeds() {
        let llm = Arc::new(FakeLlm::new(&["m"], &["m"], Unload::Works));
        llm.refuse_load(Some("m"));
        let (manager, _, _) = setup_engine(llm.clone(), false, false);
        let error = manager.run_media("comfyui", job()).await.unwrap_err();
        assert!(matches!(error, LeaseError::ReloadFailed(_)));
        assert!(!manager.gate().is_open());
        assert_eq!(manager.holder().await, Holder::Media("comfyui".into()));
        llm.refuse_load(None);
        manager.ensure_llm().await.unwrap();
        assert!(manager.gate().is_open());
        assert_eq!(manager.holder().await, Holder::Llm);
    }

    #[tokio::test]
    async fn an_engine_that_cannot_unload_keeps_the_llm_loaded_and_the_gate_open() {
        let (manager, log, transitions) = setup_llm(false, false, Unload::Unsupported);
        manager.run_media("comfyui", job()).await.unwrap();
        assert_eq!(
            log.lines(),
            vec!["comfy load", "comfy job image (gate open)", "comfy unload"]
        );
        assert!(manager.gate().is_open());
        assert_eq!(manager.holder().await, Holder::Llm);
        assert_eq!(
            transitions.lock().unwrap()[0].transition,
            "kept",
            "the journal says why the LLM stayed"
        );
    }

    #[tokio::test]
    async fn a_media_model_on_the_llms_engine_leaves_the_llm_loaded() {
        let (manager, log, transitions) = setup_media(false, false, Unload::Works, true);
        manager.run_media("comfyui", job()).await.unwrap();
        assert_eq!(
            log.lines(),
            vec!["comfy load", "comfy job image (gate open)", "comfy unload"]
        );
        assert_eq!(
            transitions.lock().unwrap()[0].detail.as_deref(),
            Some("the LLM's engine serves the media model too")
        );
    }

    #[tokio::test]
    async fn a_failed_unload_reopens_the_gate_and_skips_the_job() {
        let (manager, log, _) = setup_llm(false, false, Unload::Fails);
        let error = manager.run_media("comfyui", job()).await.unwrap_err();
        assert!(matches!(error, LeaseError::Engine(EngineError::Refused(_))));
        assert!(
            log.lines().is_empty(),
            "nothing loaded or run: {:?}",
            log.lines()
        );
        assert!(manager.gate().is_open());
        assert_eq!(manager.holder().await, Holder::Llm);
    }

    #[tokio::test]
    async fn unknown_media_and_already_loaded_llm() {
        let (manager, log, transitions) = setup(false, false);
        assert!(matches!(
            manager.run_media("tts", job()).await.unwrap_err(),
            LeaseError::UnknownMedia(_)
        ));
        manager.ensure_llm().await.unwrap();
        assert!(log.lines().is_empty(), "already loaded: nothing to do");
        assert!(records(&transitions).is_empty());
        assert_eq!(manager.media_names(), vec!["comfyui"]);
    }

    #[tokio::test]
    async fn ensure_llm_loads_each_model_that_is_not_resident() {
        let (manager, log, transitions) = setup_two(&["m"], Unload::Works, None);
        manager.ensure_llm().await.unwrap();
        assert_eq!(log.lines(), vec!["llm load r"]);
        assert_eq!(records(&transitions), ["llm load -", "llm ready -"]);
        assert!(llms_hold(&manager).await);
    }

    /// A generic engine's stop ends its server: the reload after a job
    /// still goes through `load_all`, which starts it again.
    #[tokio::test]
    async fn an_engine_the_unload_stopped_is_started_again_after_a_job() {
        let llm = FakeLlm {
            stops: true,
            ..FakeLlm::new(&["m"], &["m"], Unload::Works)
        };
        let (manager, log, _) = setup_engine(Arc::new(llm), false, false);
        let ran = manager.run_job(noting_job(&manager, &log)).await;
        assert!(ran.lease.is_ok(), "{:?}", ran.lease);
        assert_eq!(
            log.lines(),
            ["llm unload m", "job (gate open: false)", "llm load m"]
        );
    }

    /// An engine that does not answer before a session is reported, not
    /// loaded into: a start command would launch a second engine next to
    /// one that is only slow. The gate stays open.
    #[tokio::test]
    async fn ensure_llm_reports_an_engine_that_does_not_answer() {
        let llm = FakeLlm {
            down: true,
            ..two(&[], Unload::Works, None)
        };
        let (manager, log, _) = setup_engine(Arc::new(llm), false, false);
        let error = manager.ensure_llm().await.unwrap_err();
        assert!(
            matches!(error, LeaseError::Engine(EngineError::Unreachable(_))),
            "{error}"
        );
        // Requests still reach the engine and meet its retries.
        assert_eq!((log.lines(), manager.gate().is_open()), (vec![], true));
    }

    /// A failed `ensure_llm` leaves a gate closed by a failed reload closed.
    #[tokio::test]
    async fn a_failed_ensure_llm_keeps_a_closed_gate_closed() {
        let (manager, log, _) = setup(false, true);
        let ran = manager.run_job(noting_job(&manager, &log)).await;
        assert!(ran.lease.is_err(), "{:?}", ran.lease);
        assert!(manager.ensure_llm().await.is_err());
        assert!(!manager.gate().is_open());
    }

    /// A stop that fails and leaves the engine silent is not answered with
    /// a start: that could launch a second engine.
    #[tokio::test]
    async fn a_failed_unload_does_not_start_an_engine_that_does_not_answer() {
        let llm = Arc::new(FakeLlm {
            down: true,
            ..two(&["m"], Unload::Fails, None)
        });
        let (manager, log, _) = setup_engine(llm, false, false);
        let error = manager.run_media("comfyui", job()).await.unwrap_err();
        assert!(
            matches!(error, LeaseError::Engine(EngineError::Refused(_))),
            "{error}"
        );
        assert!(log.lines().is_empty(), "{:?}", log.lines());
        assert!(manager.gate().is_open());
    }

    #[tokio::test]
    async fn a_media_job_frees_every_model_and_brings_them_all_back() {
        let (manager, log, transitions) = setup_two(&["m", "r"], Unload::Works, None);
        manager.run_media("comfyui", job()).await.unwrap();
        assert_eq!(
            log.lines(),
            vec![
                "llm unload m",
                "llm unload r",
                "comfy load",
                "comfy job image (gate closed)",
                "comfy unload",
                "llm load m",
                "llm load r"
            ]
        );
        assert!(llms_hold(&manager).await);
        assert_eq!(
            kinds(&transitions),
            vec![
                "llm unload",
                "media:comfyui load",
                "media:comfyui job",
                "media:comfyui unload",
                "llm load",
                "llm ready"
            ]
        );
    }

    /// Both models freed, the job run with requests held, both loaded back.
    const TWO_MODELS_AROUND_A_JOB: [&str; 5] = [
        "llm unload m",
        "llm unload r",
        "job (gate open: false)",
        "llm load m",
        "llm load r",
    ];

    /// A heavy job frees every model and loads them all back, as a media
    /// job does.
    #[tokio::test]
    async fn a_heavy_job_frees_every_model_and_brings_them_all_back() {
        let (manager, log, _) = setup_two(&["m", "r"], Unload::Works, None);
        let ran = manager.run_job(noting_job(&manager, &log)).await;
        let after = (ran.lease.is_ok(), ran.output);
        assert_eq!(after, (true, Some(7)), "{:?}", ran.lease);
        assert_eq!(log.lines(), TWO_MODELS_AROUND_A_JOB);
        assert!(llms_hold(&manager).await);
    }

    /// A heavy job keeps its output when one model does not reload; the
    /// gate stays closed until `ensure_llm` loads that one.
    #[tokio::test]
    async fn a_heavy_job_whose_second_model_does_not_reload_keeps_its_output() {
        let llm = Arc::new(two(&["m", "r"], Unload::Works, Some("r")));
        let (manager, log, _) = setup_engine(llm.clone(), false, false);
        let ran = manager.run_job(noting_job(&manager, &log)).await;
        let reload_failed = matches!(ran.lease, Err(LeaseError::ReloadFailed(_)));
        let after = (ran.output, reload_failed, manager.gate().is_open());
        assert_eq!(after, (Some(7), true, false), "{:?}", ran.lease);
        llm.refuse_load(None);
        manager.ensure_llm().await.unwrap();
        assert_eq!(log.lines(), TWO_MODELS_AROUND_A_JOB);
        assert!(llms_hold(&manager).await);
    }

    /// The first model is freed when the second refuses: it is loaded back
    /// before requests flow again, and the job does not run.
    #[tokio::test]
    async fn a_failed_unload_of_one_model_brings_the_others_back() {
        let (manager, log, transitions) = setup_two(&["m", "r"], Unload::FailsFor("r"), None);
        let error = manager.run_media("comfyui", job()).await.unwrap_err();
        assert!(
            matches!(&error, LeaseError::Engine(EngineError::Refused(m)) if m == "r: busy"),
            "{error}"
        );
        assert_eq!(log.lines(), vec!["llm unload m", "llm load m"]);
        assert!(llms_hold(&manager).await);
        let failure = transitions.lock().unwrap()[0].detail.clone();
        assert_eq!(failure.as_deref(), Some("unload: engine refused: r: busy"));
        assert_eq!(
            kinds(&transitions)[..],
            ["llm failed", "llm load", "llm ready"]
        );
    }

    /// A reload that fails on one model keeps the gate closed; `ensure_llm`
    /// then loads only that one and gives the lease back to the LLMs.
    #[tokio::test]
    async fn a_failed_reload_of_one_model_is_finished_by_ensure_llm() {
        let llm = Arc::new(two(&["m", "r"], Unload::Works, Some("r")));
        let (manager, log, _) = setup_engine(llm.clone(), false, false);
        let error = manager.run_media("comfyui", job()).await.unwrap_err();
        assert!(matches!(error, LeaseError::ReloadFailed(_)), "{error}");
        assert!(!manager.gate().is_open());
        assert_eq!(manager.holder().await, Holder::Media("comfyui".into()));
        llm.refuse_load(None);
        manager.ensure_llm().await.unwrap();
        // `r` was refused before it was logged: one load of each.
        assert_eq!(
            log.lines(),
            vec![
                "llm unload m",
                "llm unload r",
                "comfy load",
                "comfy job image (gate closed)",
                "comfy unload",
                "llm load m",
                "llm load r"
            ]
        );
        assert!(llms_hold(&manager).await);
    }

    #[tokio::test]
    async fn an_engine_that_cannot_unload_keeps_every_model() {
        let (manager, log, _) = setup_two(&["m", "r"], Unload::Unsupported, None);
        manager.run_media("comfyui", job()).await.unwrap();
        assert_eq!(
            log.lines(),
            vec!["comfy load", "comfy job image (gate open)", "comfy unload"]
        );
        assert!(llms_hold(&manager).await);
    }

    #[tokio::test]
    async fn gate_wait_open_resolves_when_opened() {
        let gate = Gate::new();
        gate.close();
        let g = gate.clone();
        let waiter = tokio::spawn(async move {
            g.wait_open().await;
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        gate.open();
        waiter.await.unwrap();
    }
}
