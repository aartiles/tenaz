//! The application service: runs a project from its ledger to the end,
//! session by session, through the ports.

mod init;
mod phases;
mod preflight;
mod sampler;
mod session;
mod status;

use std::io;
use std::sync::Arc;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub use init::{InitOptions, Templates, init};
pub use sampler::Sampler;
pub use status::status_text;

use crate::ledger::{JournalRecord, Ledger, Phase};
use crate::params::RunParams;
use crate::ports::{
    Clock, EngineAdapter, EngineMetricsSource, Harness, HarnessError, MemoryProbe, Notifier, Vcs,
    VcsError, VerifyRunner,
};
use crate::project::Project;
use crate::util::sha256_hex;

/// The ports a run needs.
#[derive(Clone)]
pub struct Deps {
    pub harness: Arc<dyn Harness>,
    pub vcs: Arc<dyn Vcs>,
    pub verify: Arc<dyn VerifyRunner>,
    pub clock: Arc<dyn Clock>,
    pub notifier: Arc<dyn Notifier>,
    /// The LLM engine, to check that the model is served before a session.
    pub engine: Option<Arc<dyn EngineAdapter>>,
    /// The lease over the engines; before a session it makes sure the LLM is resident.
    pub lease: Option<Arc<crate::lease::LeaseManager>>,
    pub engine_metrics: Option<Arc<dyn EngineMetricsSource>>,
    pub memory: Option<Arc<dyn MemoryProbe>>,
}

impl std::fmt::Debug for Deps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deps")
            .field("harness", &self.harness.info())
            .finish_non_exhaustive()
    }
}

/// Why a run stopped before its end.
#[derive(Debug, Error)]
pub enum RunError {
    /// Ctrl-C or SIGTERM: the run pauses where it is.
    #[error("paused during {0}")]
    Paused(String),
    /// Engine failures exhausted their retries: the run blocks.
    #[error("engine unavailable: {0}")]
    EngineUnavailable(String),
    #[error(transparent)]
    Harness(#[from] HarnessError),
    #[error("git: {0}")]
    Vcs(#[from] VcsError),
    #[error("i/o: {0}")]
    Io(#[from] io::Error),
    /// A precondition the user must fix (missing files, changed pins).
    #[error("{0}")]
    Precondition(String),
}

/// Options for `run`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunOptions {
    /// Continue a BLOCKED run.
    pub resume: bool,
    /// Record a failed preflight and run anyway.
    pub allow_weak_engine: bool,
    /// Copy changed templates into a started run.
    pub refresh_templates: Option<Templates>,
    /// The version string of this binary, journaled when it changes.
    pub runner_version: String,
}

/// A run in progress.
pub struct Runner {
    project: Project,
    deps: Deps,
    ledger: Ledger,
    params: RunParams,
    stop: CancellationToken,
    sampler: Option<Sampler>,
}

impl std::fmt::Debug for Runner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner")
            .field("project", &self.project)
            .field("phase", &self.ledger.phase)
            .finish_non_exhaustive()
    }
}

impl Runner {
    /// Loads the ledger and checks the pins. `stop` pauses the run when cancelled.
    pub fn open(project: Project, deps: Deps, stop: CancellationToken) -> Result<Self, RunError> {
        if !project.exists(".agent/run.json") {
            return Err(RunError::Precondition(
                "No .agent/run.json; run init first".to_owned(),
            ));
        }
        let ledger = project.load_ledger()?;
        let params = ledger.params.clone();
        Ok(Self {
            project,
            deps,
            ledger,
            params,
            stop,
            sampler: None,
        })
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    fn stopping(&self) -> bool {
        self.stop.is_cancelled()
    }

    fn now(&self) -> String {
        self.deps.clock.now_rfc3339()
    }

    /// Writes to the run log (stdout through tracing, and `.agent/private/orchestrator.log`).
    pub(crate) fn log(&self, message: &str) {
        tracing::info!("{message}");
        if self.project.exists(".agent/private") {
            let _ = self.project.append(
                ".agent/private/orchestrator.log",
                &format!("[{}] {message}\n", self.now()),
            );
        }
    }

    pub(crate) fn journal(&self, record: JournalRecord) {
        if let Err(error) = self.project.append_journal(self.now(), record) {
            tracing::error!("cannot write the journal: {error}");
        }
    }

    pub(crate) fn save(&self) -> Result<(), RunError> {
        Ok(self.project.save_ledger(&self.ledger)?)
    }

    fn notify(&self, message: &str) {
        if self.params.notify {
            self.deps.notifier.notify(message);
        }
    }

    fn block(&mut self, reason: &str) -> Result<(), RunError> {
        self.ledger.phase_before_block = Some(self.ledger.phase);
        self.ledger.phase = Phase::Blocked;
        self.ledger.blocked_reason = Some(reason.to_owned());
        self.save()?;
        self.journal(JournalRecord::Run {
            outcome: "BLOCKED".to_owned(),
            reason: Some(reason.to_owned()),
        });
        self.log(&format!("BLOCKED: {reason}"));
        self.notify(&format!("Blocked: {reason}"));
        Ok(())
    }

    fn finish(&mut self, phase: Phase) -> Result<(), RunError> {
        self.ledger.phase = phase;
        self.ledger.ended_at = Some(self.now());
        self.ledger.checkpoint = Some(self.deps.vcs.commit_all(&format!("tenaz: {phase}"))?);
        self.save()?;
        self.journal(JournalRecord::Run {
            outcome: phase.to_string(),
            reason: None,
        });
        self.log(&format!("Run finished: {phase}"));
        self.notify(&format!("Finished: {phase}. The engine is free."));
        Ok(())
    }

    /// Checks that the pinned skills in `.agent/skills/` are unchanged.
    fn check_skills(&self) -> Result<(), RunError> {
        for skill in &self.ledger.skills {
            let dir = self.project.path(&format!(".agent/skills/{}", skill.name));
            if init::tree_hash(&dir)? != skill.sha256 {
                self.journal(JournalRecord::SkillChanged {
                    skill: skill.name.clone(),
                });
                return Err(RunError::Precondition(format!(
                    "The pinned skill {} in .agent/skills/ changed during the run; it must stay read-only",
                    skill.name
                )));
            }
        }
        Ok(())
    }

    /// Waits for the engine to serve the pinned model (up to 30 tries, 20 s apart), unless stopping.
    async fn wait_for_engine(&self) -> bool {
        for _ in 0..30 {
            if self.stopping() {
                return false;
            }
            match self.engine_serving().await {
                Ok(()) => return true,
                Err(_) => {
                    self.deps
                        .clock
                        .sleep(std::time::Duration::from_secs(20))
                        .await;
                }
            }
        }
        false
    }

    /// Whether the engine lists the pinned model as loaded and ready. With a
    /// lease, the LLM is loaded first when it is not resident (after a media
    /// job whose reload failed, for example).
    async fn engine_serving(&self) -> Result<(), RunError> {
        if let Some(lease) = &self.deps.lease {
            lease
                .ensure_llm()
                .await
                .map_err(|e| RunError::Precondition(format!("the LLM could not be loaded: {e}")))?;
        }
        let Some(engine) = &self.deps.engine else {
            return Ok(());
        };
        let id = &self.ledger.config.id;
        let models = engine.models().await.map_err(|e| {
            RunError::Precondition(format!(
                "Model {id} is not being served ({e}). Start the inference server first."
            ))
        })?;
        if models.iter().any(|m| &m.id == id && m.loaded) {
            Ok(())
        } else {
            Err(RunError::Precondition(format!(
                "Model {id} is not being served at {}. Start the inference server first.",
                self.ledger.config.base_url.clone().unwrap_or_default()
            )))
        }
    }

    /// The whole run: preconditions, preflight, then the phases until the run
    /// ends, blocks or pauses.
    pub async fn run(&mut self, options: RunOptions) -> Result<(), RunError> {
        let project = self.project.clone();
        project.create_layout()?;
        let brief = project
            .read("BRIEF.md")
            .map_err(|_| RunError::Precondition("BRIEF.md is missing".to_owned()))?;
        if sha256_hex(&brief) != self.ledger.brief_sha256 {
            return Err(RunError::Precondition(
                "BRIEF.md changed since init".to_owned(),
            ));
        }
        self.check_skills()?;
        let info = self.deps.harness.info();
        if info.version != self.ledger.config.harness_version {
            return Err(RunError::Precondition(format!(
                "Pinned harness version changed ({} -> {})",
                self.ledger.config.harness_version, info.version
            )));
        }
        if self.ledger.phase == Phase::Blocked && options.resume {
            self.ledger.phase = self.ledger.phase_before_block.take().unwrap_or(Phase::Loop);
            self.ledger.blocked_reason = None;
            self.save()?;
        }
        if let Some(templates) = &options.refresh_templates {
            init::refresh_templates(&self.project, self.deps.vcs.as_ref(), templates)?;
            let to = templates.sha256();
            self.journal(JournalRecord::TemplatesRefreshed {
                from: Some(self.ledger.templates_sha256.clone()),
                to: to.clone(),
            });
            self.ledger.templates_sha256 = to;
            self.save()?;
        }
        if options.runner_version != self.ledger.runner_version {
            self.journal(JournalRecord::RunnerChanged {
                from: Some(self.ledger.runner_version.clone()),
                to: options.runner_version.clone(),
            });
            self.ledger
                .runner_version
                .clone_from(&options.runner_version);
            self.save()?;
        }
        if !self.ledger.phase.is_running() {
            let reason = self
                .ledger
                .blocked_reason
                .clone()
                .map(|r| format!(": {r}"))
                .unwrap_or_default();
            self.log(&format!("Run is {}{reason}", self.ledger.phase));
            return Ok(());
        }
        self.engine_serving().await?;
        if !self.ledger.preflight.as_ref().is_some_and(|p| p.ok) {
            let preflight = self.preflight().await?;
            self.log(&format!(
                "Preflight: reasoning={} cache={}",
                preflight.reasoning, preflight.cache
            ));
            let ok = preflight.ok;
            self.ledger.preflight = Some(preflight);
            self.save()?;
            if !ok {
                if !options.allow_weak_engine {
                    return Err(RunError::Precondition("Engine preflight failed (see .agent/run.json \"preflight\"). Fix thinking/prefix cache, or pass --allow-weak-engine to record the run anyway.".to_owned()));
                }
                if let Some(p) = &mut self.ledger.preflight {
                    p.accepted_weak = true;
                    p.ok = true;
                }
                self.save()?;
            }
        }
        if let Some(inflight) = self.ledger.inflight.take() {
            self.journal(JournalRecord::Interrupted {
                label: inflight.label.clone(),
                role: inflight.role,
                task: inflight.task.clone(),
                started_at: inflight.started_at.clone(),
            });
            self.log(&format!(
                "Previous session {} was interrupted; continuing",
                inflight.label
            ));
            self.save()?;
        }
        if let Some(paused_at) = self.ledger.paused_at.take() {
            self.journal(JournalRecord::Resumed {
                paused_at: paused_at.clone(),
            });
            self.log(&format!("Resuming after a pause since {paused_at}"));
            self.save()?;
        }
        if let (Some(probe), true) = (&self.deps.memory, self.params.memory_sample_sec > 0) {
            let sampler = Sampler::new(
                probe.clone(),
                self.deps.clock.clone(),
                self.project.clone(),
                std::time::Duration::from_secs(self.params.memory_sample_sec),
            );
            sampler.start();
            self.sampler = Some(sampler);
        }
        self.notify(&format!(
            "Started ({}). It holds the engine and most memory until it ends.",
            self.ledger.phase
        ));
        let outcome = self.phases().await;
        if let Some(sampler) = self.sampler.take() {
            sampler.stop();
        }
        match outcome {
            Ok(()) => {
                if self.stopping() && self.ledger.phase.is_running() {
                    self.ledger.paused_at = Some(self.now());
                    self.save()?;
                    self.journal(JournalRecord::Paused {
                        label: None,
                        role: None,
                        task: None,
                        attempt: None,
                    });
                    self.log(
                        "PAUSED between sessions. Run again (no --resume needed) to continue.",
                    );
                }
                Ok(())
            }
            Err(RunError::Paused(label)) => {
                self.log(&format!("PAUSED during {label}; its changes are kept. Run again (no --resume needed) to continue."));
                self.notify("Paused. The engine is free; run again to continue.");
                Ok(())
            }
            Err(RunError::EngineUnavailable(reason)) => self.block(&format!(
                "engine unavailable: {reason}. Fix the engine, then run with --resume"
            )),
            Err(error) => {
                self.ledger.last_error = Some(error.to_string());
                self.ledger.inflight = None;
                let _ = self.save();
                self.notify(&format!("Stopped with an error: {error}"));
                Err(error)
            }
        }
    }

    async fn phases(&mut self) -> Result<(), RunError> {
        while !self.stopping() && self.ledger.phase.is_running() {
            match self.ledger.phase {
                Phase::Bootstrap => self.bootstrap_phase().await?,
                Phase::Loop => self.loop_phase().await?,
                _ => self.final_phase().await?,
            }
        }
        Ok(())
    }
}
