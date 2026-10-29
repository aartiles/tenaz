//! The lease: one resident engine. The LLM holds it by default; a media job
//! borrows it (unload LLM, load media, run, unload media, reload LLM). The
//! proxy's gate holds LLM requests while the LLM is away. An engine that
//! cannot unload keeps the LLM loaded, and the job runs next to it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use thiserror::Error;
use tokio::sync::{Mutex, watch};

use crate::ports::{
    EngineAdapter, EngineError, MediaAdapter, MediaError, MediaJob, MediaOutput, serves,
};

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
}

impl std::fmt::Display for Holder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Holder::Llm => f.write_str("llm"),
            Holder::Media(name) => write!(f, "media:{name}"),
        }
    }
}

/// One step of a lease cycle, with its timing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseTransition {
    pub holder: Holder,
    /// `unload`, `load`, `job`, `ready`, `kept`, `failed`.
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
    /// The job ran, but the LLMs could not be reloaded afterwards.
    #[error("the LLMs did not reload after the media job: {0}")]
    ReloadFailed(EngineError),
}

/// Observes transitions (the orchestrator journals them).
pub type Observer = Arc<dyn Fn(LeaseTransition) + Send + Sync>;

/// The lease manager.
pub struct LeaseManager {
    llm: Arc<dyn EngineAdapter>,
    /// The run's model first, then the other roles' models (ADR 0010).
    llm_models: Vec<String>,
    media: BTreeMap<String, Arc<dyn MediaAdapter>>,
    gate: Gate,
    holder: Mutex<Holder>,
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
    /// A lease over `llm_models`, the LLMs on `llm` that must all be
    /// resident while sessions run.
    pub fn new(
        llm: Arc<dyn EngineAdapter>,
        llm_models: Vec<String>,
        gate: Gate,
        observer: Observer,
    ) -> Self {
        Self {
            llm,
            llm_models,
            media: BTreeMap::new(),
            gate,
            holder: Mutex::new(Holder::Llm),
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
        self.holder.lock().await.clone()
    }

    /// Loads the LLMs the engine does not list as ready. Used before the
    /// first session and after a failed reload.
    /// An engine that does not answer is an error here, not a reason to
    /// start it.
    pub async fn ensure_llm(&self) -> Result<(), LeaseError> {
        let mut holder = self.holder.lock().await;
        if !self.llms_ready().await? {
            self.load_llms().await?;
        }
        *holder = Holder::Llm;
        self.gate.open();
        Ok(())
    }

    /// Whether the engine lists every LLM as loaded and ready.
    async fn llms_ready(&self) -> Result<bool, EngineError> {
        let listed = self.llm.models().await?;
        Ok(self.llm_models.iter().all(|id| serves(&listed, id)))
    }

    async fn load_llms(&self) -> Result<(), EngineError> {
        self.timed(Holder::Llm, "load", self.llm.load_all(&self.llm_models))
            .await
    }

    /// The whole cycle for one media job. The holder lock serialises cycles.
    pub async fn run_media(&self, name: &str, job: MediaJob) -> Result<MediaOutput, LeaseError> {
        let adapter = self
            .media
            .get(name)
            .cloned()
            .ok_or_else(|| LeaseError::UnknownMedia(name.to_owned()))?;
        let mut holder = self.holder.lock().await;
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
        self.gate.close();
        let unloaded = self
            .timed(Holder::Llm, "unload", self.llm.unload_all(&self.llm_models))
            .await;
        if let Err(error) = unloaded {
            // Some LLMs may be freed already: load them back while the
            // engine answers. One that does not answer is not started here,
            // where it may only be slow; requests meet the engine retry like
            // any engine failure instead of waiting here.
            if let Ok(false) = self.llms_ready().await {
                let _ = self.load_llms().await;
            }
            self.gate.open();
            return Err(error.into());
        }
        *holder = media_holder.clone();
        let result = self.media_cycle(&media_holder, adapter.as_ref(), job).await;
        let reload = self.load_llms().await;
        match reload {
            Ok(()) => {
                *holder = Holder::Llm;
                self.gate.open();
                self.observe(Holder::Llm, "ready", 0, None);
            }
            Err(error) => {
                self.observe(Holder::Llm, "failed", 0, Some(error.to_string()));
                // The gate stays closed: requests wait until `ensure_llm` succeeds.
                return match result {
                    Ok(_) | Err(_) => Err(LeaseError::ReloadFailed(error)),
                };
            }
        }
        result
    }

    async fn media_cycle(
        &self,
        holder: &Holder,
        adapter: &dyn MediaAdapter,
        job: MediaJob,
    ) -> Result<MediaOutput, LeaseError> {
        self.timed(holder.clone(), "load", adapter.load()).await?;
        let output = self.timed(holder.clone(), "job", adapter.run(job)).await;
        // Always free the media engine, even after a failed job.
        let unloaded = self.timed(holder.clone(), "unload", adapter.unload()).await;
        let output = output?;
        unloaded?;
        Ok(output)
    }

    async fn timed<T, E: std::fmt::Display>(
        &self,
        holder: Holder,
        transition: &str,
        op: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        let started = Instant::now();
        let result = op.await;
        let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match &result {
            Ok(_) => self.observe(holder, transition, ms, None),
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
    use std::collections::BTreeSet;
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;

    use super::*;
    use crate::ports::ModelState;

    #[derive(Default)]
    struct Log(StdMutex<Vec<String>>);
    impl Log {
        fn push(&self, s: impl Into<String>) {
            self.0.lock().unwrap().push(s.into());
        }
        fn lines(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    struct FakeLlm {
        log: Arc<Log>,
        /// The models the engine lists.
        listed: Vec<String>,
        loaded: StdMutex<BTreeSet<String>>,
        fail_reload: bool,
        /// This model does not load.
        fail_load: StdMutex<Option<&'static str>>,
        /// The engine does not answer.
        down: bool,
        unload: Unload,
    }

    impl FakeLlm {
        /// Whether a load of `model` fails: it is the one that never loads,
        /// or the engine fails reloads and something was unloaded.
        fn refuses(&self, model: &str) -> bool {
            if *self.fail_load.lock().unwrap() == Some(model) {
                return true;
            }
            self.fail_reload && self.log.lines().iter().any(|l| l.starts_with("llm unload"))
        }
    }

    /// How the fake LLM engine answers an unload.
    #[derive(Clone, Copy)]
    enum Unload {
        Works,
        Fails,
        /// Only this model's unload fails.
        FailsFor(&'static str),
        /// The engine has no way to unload (a generic engine without commands).
        Unsupported,
    }
    #[async_trait]
    impl EngineAdapter for FakeLlm {
        fn name(&self) -> &str {
            "fake-llm"
        }
        async fn models(&self) -> Result<Vec<ModelState>, EngineError> {
            if self.down {
                return Err(EngineError::Unreachable("connection refused".into()));
            }
            let loaded = self.loaded.lock().unwrap();
            Ok(self
                .listed
                .iter()
                .map(|id| ModelState {
                    id: id.clone(),
                    loaded: loaded.contains(id),
                    ready: loaded.contains(id),
                    context_length: Some(131072),
                })
                .collect())
        }
        async fn load(&self, model: &str) -> Result<(), EngineError> {
            if self.refuses(model) {
                return Err(EngineError::Refused("no memory".into()));
            }
            self.log.push(format!("llm load {model}"));
            self.loaded.lock().unwrap().insert(model.to_owned());
            Ok(())
        }
        async fn unload(&self, model: &str) -> Result<(), EngineError> {
            match self.unload {
                Unload::Fails => return Err(EngineError::Refused("stop command failed".into())),
                Unload::FailsFor(id) if id == model => {
                    return Err(EngineError::Refused("busy".into()));
                }
                _ => {}
            }
            self.log.push(format!("llm unload {model}"));
            self.loaded.lock().unwrap().remove(model);
            Ok(())
        }
        fn can_unload(&self) -> bool {
            !matches!(self.unload, Unload::Unsupported)
        }
    }

    struct FakeComfy {
        log: Arc<Log>,
        fail_job: bool,
        gate: Gate,
        shares_engine: bool,
    }
    #[async_trait]
    impl MediaAdapter for FakeComfy {
        fn name(&self) -> &str {
            "comfyui"
        }
        async fn load(&self) -> Result<(), MediaError> {
            self.log.push("comfy load");
            Ok(())
        }
        async fn run(&self, job: MediaJob) -> Result<MediaOutput, MediaError> {
            self.log.push(format!(
                "comfy job {} (gate {})",
                job.kind,
                if self.gate.is_open() {
                    "open"
                } else {
                    "closed"
                }
            ));
            if self.fail_job {
                return Err(MediaError::Failed("bad workflow".into()));
            }
            Ok(MediaOutput {
                paths: vec![job.output_dir.join("out.png")],
                detail: serde_json::json!({}),
            })
        }
        async fn unload(&self) -> Result<(), MediaError> {
            self.log.push("comfy unload");
            Ok(())
        }
        fn shares_engine(&self) -> bool {
            self.shares_engine
        }
    }

    type Setup = (LeaseManager, Arc<Log>, Arc<StdMutex<Vec<LeaseTransition>>>);

    fn setup(fail_job: bool, fail_reload: bool) -> Setup {
        setup_llm(fail_job, fail_reload, Unload::Works)
    }

    fn setup_llm(fail_job: bool, fail_reload: bool, unload: Unload) -> Setup {
        setup_media(fail_job, fail_reload, unload, false)
    }

    fn setup_media(fail_job: bool, fail_reload: bool, unload: Unload, shares: bool) -> Setup {
        let llm = FakeLlm {
            log: Arc::default(),
            listed: vec!["m".into()],
            loaded: StdMutex::new(BTreeSet::from(["m".to_owned()])),
            fail_reload,
            fail_load: StdMutex::new(None),
            down: false,
            unload,
        };
        setup_engine(Arc::new(llm), fail_job, shares)
    }

    /// An engine listing `m` and `r`, of which `loaded` are resident;
    /// `fail_load` does not load.
    fn two(loaded: &[&str], unload: Unload, fail_load: Option<&'static str>) -> FakeLlm {
        FakeLlm {
            log: Arc::default(),
            listed: vec!["m".into(), "r".into()],
            loaded: StdMutex::new(loaded.iter().map(|m| (*m).to_owned()).collect()),
            fail_reload: false,
            fail_load: StdMutex::new(fail_load),
            down: false,
            unload,
        }
    }

    fn setup_two(loaded: &[&str], unload: Unload, fail_load: Option<&'static str>) -> Setup {
        setup_engine(Arc::new(two(loaded, unload, fail_load)), false, false)
    }

    fn setup_engine(llm: Arc<FakeLlm>, fail_job: bool, shares: bool) -> Setup {
        let log = llm.log.clone();
        let transitions = Arc::new(StdMutex::new(Vec::new()));
        let seen = transitions.clone();
        let observer: Observer = Arc::new(move |t| seen.lock().unwrap().push(t));
        let models = llm.listed.clone();
        let gate = Gate::new();
        let manager = LeaseManager::new(llm, models, gate.clone(), observer).with_media(Arc::new(
            FakeComfy {
                log: log.clone(),
                fail_job,
                gate,
                shares_engine: shares,
            },
        ));
        (manager, log, transitions)
    }

    fn job() -> MediaJob {
        MediaJob {
            kind: "image".into(),
            params: serde_json::json!({"prompt": "a cat"}),
            output_dir: "/tmp/out".into(),
        }
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
        let (manager, log, _) = setup(false, true);
        let error = manager.run_media("comfyui", job()).await.unwrap_err();
        assert!(matches!(error, LeaseError::ReloadFailed(_)));
        assert!(!manager.gate().is_open());
        assert_eq!(manager.holder().await, Holder::Media("comfyui".into()));
        // The fake refuses to load while an "llm unload" is in its log; clear it and retry.
        log.0
            .lock()
            .unwrap()
            .retain(|l| !l.starts_with("llm unload"));
        manager.ensure_llm().await.unwrap();
        assert!(manager.gate().is_open());
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
        let (manager, log, _) = setup(false, false);
        assert!(matches!(
            manager.run_media("tts", job()).await.unwrap_err(),
            LeaseError::UnknownMedia(_)
        ));
        manager.ensure_llm().await.unwrap();
        assert!(log.lines().is_empty(), "already loaded: nothing to do");
        assert_eq!(manager.media_names(), vec!["comfyui"]);
    }

    /// Whether the LLMs hold the lease with the gate open.
    async fn llms_hold(manager: &LeaseManager) -> bool {
        manager.gate().is_open() && manager.holder().await == Holder::Llm
    }

    fn kinds(transitions: &StdMutex<Vec<LeaseTransition>>) -> Vec<String> {
        transitions
            .lock()
            .unwrap()
            .iter()
            .map(|t| format!("{} {}", t.holder, t.transition))
            .collect()
    }

    #[tokio::test]
    async fn ensure_llm_loads_each_model_that_is_not_resident() {
        let (manager, log, _) = setup_two(&["m"], Unload::Works, None);
        manager.ensure_llm().await.unwrap();
        assert_eq!(log.lines(), vec!["llm load r"]);
        assert!(llms_hold(&manager).await);
    }

    /// An engine that does not answer before a session is reported, not
    /// loaded into: a start command would launch a second engine next to
    /// one that is only slow.
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
        assert!(log.lines().is_empty(), "{:?}", log.lines());
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
        assert_eq!(kinds(&transitions)[..], ["llm failed", "llm load"]);
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
        *llm.fail_load.lock().unwrap() = None;
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
