//! A fake LLM engine and a fake media engine for the lease's tests, with
//! a log of what each was asked to do.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;

use super::{Gate, LeaseManager, LeaseTransition, Observer};
use crate::ports::{
    EngineAdapter, EngineError, MediaAdapter, MediaError, MediaJob, MediaOutput, ModelState,
};

#[derive(Default)]
pub(super) struct Log(StdMutex<Vec<String>>);

impl Log {
    pub(super) fn push(&self, s: impl Into<String>) {
        self.0.lock().unwrap().push(s.into());
    }

    pub(super) fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    pub(super) fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

pub(super) struct FakeLlm {
    pub(super) log: Arc<Log>,
    /// The models the engine lists.
    pub(super) listed: Vec<String>,
    pub(super) loaded: StdMutex<BTreeSet<String>>,
    /// Listed as loaded, but not ready yet.
    pub(super) warming: StdMutex<BTreeSet<String>>,
    /// This model does not load.
    pub(super) fail_load: StdMutex<Option<&'static str>>,
    /// The engine does not answer.
    pub(super) down: bool,
    /// The engine does not answer once no model is loaded: a stop
    /// command ended its server.
    pub(super) stops: bool,
    pub(super) unload: Unload,
    /// A gate whose state each load notes in the log.
    pub(super) watched: StdMutex<Option<Gate>>,
}

impl FakeLlm {
    /// An engine listing `listed`, of which `loaded` are resident.
    pub(super) fn new(listed: &[&str], loaded: &[&str], unload: Unload) -> Self {
        let set = |ids: &[&str]| ids.iter().map(|id| (*id).to_owned()).collect();
        Self {
            log: Arc::default(),
            listed: listed.iter().map(|id| (*id).to_owned()).collect(),
            loaded: StdMutex::new(set(loaded)),
            warming: StdMutex::default(),
            fail_load: StdMutex::default(),
            down: false,
            stops: false,
            unload,
            watched: StdMutex::default(),
        }
    }

    fn refuses(&self, model: &str) -> bool {
        *self.fail_load.lock().unwrap() == Some(model)
    }

    /// From now on, `model` does not load; `None` lets every model load.
    pub(super) fn refuse_load(&self, model: Option<&'static str>) {
        *self.fail_load.lock().unwrap() = model;
    }

    /// The engine drops every model by itself.
    pub(super) fn drop_all(&self) {
        self.loaded.lock().unwrap().clear();
    }

    /// The engine loads `model` by itself.
    pub(super) fn take_up(&self, model: &str) {
        self.loaded.lock().unwrap().insert(model.to_owned());
    }

    /// From now on, each load notes whether `gate` was open.
    pub(super) fn watch(&self, gate: &Gate) {
        *self.watched.lock().unwrap() = Some(gate.clone());
    }

    /// `model` is loading: listed as loaded, not yet ready.
    pub(super) fn warm(&self, model: &str) {
        self.warming.lock().unwrap().insert(model.to_owned());
    }
}

/// How the fake LLM engine answers an unload.
#[derive(Clone, Copy)]
pub(super) enum Unload {
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
        let loaded = self.loaded.lock().unwrap();
        let stopped = self.stops && loaded.is_empty();
        if self.down || stopped {
            return Err(EngineError::Unreachable("connection refused".into()));
        }
        let warming = self.warming.lock().unwrap();
        Ok(self
            .listed
            .iter()
            .map(|id| ModelState {
                id: id.clone(),
                loaded: loaded.contains(id) || warming.contains(id),
                ready: loaded.contains(id),
                context_length: Some(131072),
            })
            .collect())
    }

    async fn load(&self, model: &str) -> Result<(), EngineError> {
        if self.refuses(model) {
            return Err(EngineError::Refused("no memory".into()));
        }
        let gate = self.watched.lock().unwrap().as_ref().map(|gate| {
            let state = if gate.is_open() { "open" } else { "closed" };
            format!(" (gate {state})")
        });
        self.log
            .push(format!("llm load {model}{}", gate.unwrap_or_default()));
        self.warming.lock().unwrap().remove(model);
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
        self.warming.lock().unwrap().remove(model);
        self.loaded.lock().unwrap().remove(model);
        Ok(())
    }

    fn can_unload(&self) -> bool {
        !matches!(self.unload, Unload::Unsupported)
    }

    fn unloads_each_model(&self) -> bool {
        self.can_unload() && !self.stops
    }
}

struct FakeComfy {
    log: Arc<Log>,
    fail_job: bool,
    fail_unload: bool,
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
        let gate = if self.gate.is_open() {
            "open"
        } else {
            "closed"
        };
        self.log
            .push(format!("comfy job {} (gate {gate})", job.kind));
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
        if self.fail_unload {
            return Err(MediaError::Unreachable("connection refused".into()));
        }
        Ok(())
    }

    fn shares_engine(&self) -> bool {
        self.shares_engine
    }
}

pub(super) type Setup = (LeaseManager, Arc<Log>, Arc<StdMutex<Vec<LeaseTransition>>>);

/// A lease over `m`, loaded; with `fail_reload`, `m` does not load again
/// once unloaded.
pub(super) fn setup(fail_job: bool, fail_reload: bool) -> Setup {
    setup_llm(fail_job, fail_reload, Unload::Works)
}

pub(super) fn setup_llm(fail_job: bool, fail_reload: bool, unload: Unload) -> Setup {
    setup_media(fail_job, fail_reload, unload, false)
}

pub(super) fn setup_media(
    fail_job: bool,
    fail_reload: bool,
    unload: Unload,
    shares: bool,
) -> Setup {
    let llm = FakeLlm::new(&["m"], &["m"], unload);
    if fail_reload {
        llm.refuse_load(Some("m"));
    }
    setup_engine(Arc::new(llm), fail_job, shares)
}

/// An engine listing `m` and `r`, of which `loaded` are resident;
/// `fail_load` does not load.
pub(super) fn two(loaded: &[&str], unload: Unload, fail_load: Option<&'static str>) -> FakeLlm {
    let llm = FakeLlm::new(&["m", "r"], loaded, unload);
    llm.refuse_load(fail_load);
    llm
}

pub(super) fn setup_two(loaded: &[&str], unload: Unload, fail_load: Option<&'static str>) -> Setup {
    setup_engine(Arc::new(two(loaded, unload, fail_load)), false, false)
}

/// A lease whose run's models are every model `llm` lists.
pub(super) fn setup_engine(llm: Arc<FakeLlm>, fail_job: bool, shares: bool) -> Setup {
    let models = llm.listed.clone();
    setup_run(llm, models, fail_job, shares)
}

/// A lease over the run's `models` on `llm`, with the fake media engine.
pub(super) fn setup_run(
    llm: Arc<FakeLlm>,
    models: Vec<String>,
    fail_job: bool,
    shares: bool,
) -> Setup {
    let media = MediaBehaviour {
        fail_job,
        fail_unload: false,
        shares,
    };
    build(llm, models, &media)
}

/// A lease over `m`, loaded, whose media engine fails to unload.
pub(super) fn setup_failing_media_unload() -> Setup {
    let llm = Arc::new(FakeLlm::new(&["m"], &["m"], Unload::Works));
    let models = llm.listed.clone();
    let media = MediaBehaviour {
        fail_job: false,
        fail_unload: true,
        shares: false,
    };
    build(llm, models, &media)
}

/// How the fake media engine behaves.
struct MediaBehaviour {
    fail_job: bool,
    fail_unload: bool,
    shares: bool,
}

fn build(llm: Arc<FakeLlm>, models: Vec<String>, fake: &MediaBehaviour) -> Setup {
    let log = llm.log.clone();
    let transitions = Arc::new(StdMutex::new(Vec::new()));
    let seen = transitions.clone();
    let observer: Observer = Arc::new(move |t| seen.lock().unwrap().push(t));
    let gate = Gate::new();
    let media = FakeComfy {
        log: log.clone(),
        fail_job: fake.fail_job,
        fail_unload: fake.fail_unload,
        gate: gate.clone(),
        shares_engine: fake.shares,
    };
    let manager = LeaseManager::new(llm, models, gate, observer).with_media(Arc::new(media));
    (manager, log, transitions)
}

pub(super) fn job() -> MediaJob {
    MediaJob {
        kind: "image".into(),
        params: serde_json::json!({"prompt": "a cat"}),
        output_dir: "/tmp/out".into(),
    }
}

/// A job that notes whether the gate was open while it ran, and ends
/// with 7.
pub(super) fn noting_job(manager: &LeaseManager, log: &Arc<Log>) -> impl Future<Output = i32> {
    let gate = manager.gate().clone();
    let log = log.clone();
    async move {
        log.push(format!("job (gate open: {})", gate.is_open()));
        7
    }
}

/// Each lease record as `holder transition`.
pub(super) fn kinds(transitions: &StdMutex<Vec<LeaseTransition>>) -> Vec<String> {
    transitions
        .lock()
        .unwrap()
        .iter()
        .map(|t| format!("{} {}", t.holder, t.transition))
        .collect()
}

/// Each lease record as `holder transition detail`, `-` for none.
pub(super) fn records(transitions: &StdMutex<Vec<LeaseTransition>>) -> Vec<String> {
    transitions
        .lock()
        .unwrap()
        .iter()
        .map(|t| {
            let detail = t.detail.as_deref().unwrap_or("-");
            format!("{} {} {detail}", t.holder, t.transition)
        })
        .collect()
}
