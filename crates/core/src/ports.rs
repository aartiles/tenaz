//! The interfaces the orchestration depends on. Adapters implement them;
//! tests use fakes.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;

use crate::memory::MemoryReading;
use crate::metrics::EngineCounters;
use crate::session::SessionEvent;
use crate::verify::RawVerifyOutput;

// ---------- harness ----------

/// Who runs the agent sessions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessInfo {
    pub name: String,
    pub version: String,
}

/// The model a session runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionModel {
    /// The model as the harness names it (`provider/id`), as in `run.json`.
    pub name: String,
    /// The model as the engine lists it.
    pub id: String,
    pub context_window: u64,
    pub supports_images: bool,
}

/// How to open one session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSpec {
    /// `003-builder-T007`; names the raw logs.
    pub label: String,
    pub cwd: PathBuf,
    /// Where the harness may keep its own session state (`.tenaz/private/sessions`).
    /// `None` asks for no persistence.
    pub persist_dir: Option<PathBuf>,
    /// Where the raw event stream is appended (`.tenaz/private/<label>.rpc.jsonl`):
    /// the harness's own records where it has a line protocol (Pi), else the
    /// translated session events (fx over ACP).
    pub raw_log: Option<PathBuf>,
    /// Where the model's reasoning text is appended
    /// (`.tenaz/private/<label>.thinking.md`), for harnesses whose raw
    /// stream does not keep it.
    pub thinking_log: Option<PathBuf>,
    /// Where the harness's stderr is appended.
    pub stderr_log: Option<PathBuf>,
    /// The model to run: the run's, or the one its role is pinned to. A
    /// harness runs this model or refuses the session.
    pub model: SessionModel,
}

/// The runner's answer to a permission request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny,
}

/// How the harness process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionExit {
    pub code: Option<i32>,
}

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error("cannot start the harness: {0}")]
    Spawn(String),
    #[error("harness protocol error: {0}")]
    Protocol(String),
    #[error("the session is closed")]
    Closed,
}

/// Opens sessions.
#[async_trait]
pub trait Harness: Send + Sync {
    fn info(&self) -> HarnessInfo;
    async fn open(&self, spec: SessionSpec) -> Result<Box<dyn Session>, HarnessError>;
}

/// One live session. Events arrive through `next_event` until the stream
/// ends; the runner answers permission requests, steers and aborts.
#[async_trait]
pub trait Session: Send {
    /// Sends a user prompt. The turn ends with `SessionEvent::Settled`.
    async fn prompt(&mut self, text: &str) -> Result<(), HarnessError>;
    /// The next event, or `None` once the harness has exited.
    async fn next_event(&mut self) -> Option<SessionEvent>;
    /// A message injected into the running turn.
    async fn steer(&mut self, text: &str) -> Result<(), HarnessError>;
    /// Stops the running turn; `Settled { aborted: true }` follows.
    async fn abort(&mut self) -> Result<(), HarnessError>;
    async fn answer_permission(
        &mut self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), HarnessError>;
    /// Ends the session and waits for the harness to exit, killing it after `grace`.
    async fn close(&mut self, grace: Duration) -> SessionExit;
}

// ---------- repository ----------

#[derive(Debug, Error)]
#[error("{0}")]
pub struct VcsError(pub String);

/// The project's git repository. Operations are short and blocking.
pub trait Vcs: Send + Sync {
    fn init(&self) -> Result<(), VcsError>;
    fn head(&self) -> Result<String, VcsError>;
    /// `git add -A` and commit when anything is staged. Returns HEAD.
    fn commit_all(&self, message: &str) -> Result<String, VcsError>;
    /// Stages only `paths` and commits when anything is staged.
    fn commit_paths(&self, paths: &[&str], message: &str) -> Result<(), VcsError>;
    /// Modified, added, deleted and untracked paths relative to the root.
    fn changed_paths(&self) -> Result<Vec<String>, VcsError>;
    fn in_head(&self, path: &str) -> bool;
    fn head_content(&self, path: &str) -> Option<String>;
    /// Restores `path` from HEAD, or deletes it when HEAD has no such path.
    fn revert_path(&self, path: &str) -> Result<(), VcsError>;
    /// `git reset --hard <sha>` and `git clean -fd`.
    fn reset_hard(&self, sha: &str) -> Result<(), VcsError>;
    /// Restores `paths` from `sha` and removes untracked files under them.
    fn restore_paths(&self, sha: &str, paths: &[&str]) -> Result<(), VcsError>;
    /// A hash of the working tree state (status plus diff).
    fn worktree_fingerprint(&self) -> Result<String, VcsError>;
    /// `git diff <from> <to>`, excluding `.tenaz/` but `.tenaz/verify` when
    /// asked.
    fn diff(&self, from: &str, to: &str, exclude_agent: bool) -> Result<String, VcsError>;
    /// `git diff --stat <from> [<to>]`.
    fn diff_stat(
        &self,
        from: &str,
        to: Option<&str>,
        exclude_agent: bool,
    ) -> Result<String, VcsError>;
}

// ---------- verify ----------

/// Runs the project's `verify` executable.
#[async_trait]
pub trait VerifyRunner: Send + Sync {
    async fn run(&self, args: &[&str], timeout: Duration) -> RawVerifyOutput;
}

// ---------- heavy jobs ----------

pub use tokio_util::sync::CancellationToken;

/// How a heavy job a builder declared ended (ADR 0010).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobOutcome {
    /// The exit code; `None` when it was killed or could not start.
    pub status: Option<i32>,
    /// It ran past its time limit and was killed.
    pub timed_out: bool,
    /// The run was stopping and it was killed.
    pub stopped: bool,
    /// Why the job could not run or be waited for.
    pub error: Option<String>,
}

/// A heavy job to run: a shell command from the project root, its time
/// limit, and the file its output goes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobSpec {
    pub command: String,
    pub timeout: Duration,
    pub log: PathBuf,
}

/// Runs a builder's heavy job, killed at its time limit or when `stop`
/// fires.
#[async_trait]
pub trait JobRunner: Send + Sync {
    async fn run(&self, job: &JobSpec, stop: &CancellationToken) -> JobOutcome;
}

// ---------- time, notifications, memory ----------

/// Wall-clock and monotonic time, and sleeping.
#[async_trait]
pub trait Clock: Send + Sync {
    /// RFC 3339 UTC, millisecond precision.
    fn now_rfc3339(&self) -> String;
    fn monotonic_ms(&self) -> u64;
    async fn sleep(&self, duration: Duration);
}

/// Desktop notifications for whoever shares the machine.
pub trait Notifier: Send + Sync {
    fn notify(&self, message: &str);
}

/// Reads the machine's memory state.
#[async_trait]
pub trait MemoryProbe: Send + Sync {
    async fn read(&self) -> Option<MemoryReading>;
}

/// Reads the engine's cumulative counters.
#[async_trait]
pub trait EngineMetricsSource: Send + Sync {
    async fn counters(&self) -> Option<EngineCounters>;
}

// ---------- engines under the lease ----------

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("engine unreachable: {0}")]
    Unreachable(String),
    #[error("engine refused: {0}")]
    Refused(String),
    #[error("engine timed out: {0}")]
    Timeout(String),
}

impl EngineError {
    /// The same error, saying it was about `model`.
    #[must_use]
    pub fn about(self, model: &str) -> Self {
        match self {
            Self::Unreachable(m) => Self::Unreachable(format!("{model}: {m}")),
            Self::Refused(m) => Self::Refused(format!("{model}: {m}")),
            Self::Timeout(m) => Self::Timeout(format!("{model}: {m}")),
        }
    }
}

/// What the engine reports about one model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelState {
    pub id: String,
    pub loaded: bool,
    pub ready: bool,
    pub context_length: Option<u64>,
}

impl ModelState {
    /// Loaded and ready to serve requests.
    pub fn serving(&self) -> bool {
        self.loaded && self.ready
    }
}

/// Whether `listed` has `id` loaded and ready.
pub fn serves(listed: &[ModelState], id: &str) -> bool {
    listed.iter().any(|m| m.id == id && m.serving())
}

/// An LLM engine the runner can load and unload.
#[async_trait]
pub trait EngineAdapter: Send + Sync {
    fn name(&self) -> &str;
    /// Reachable, and which models are loaded.
    async fn models(&self) -> Result<Vec<ModelState>, EngineError>;
    /// Returns when the model is ready to serve.
    async fn load(&self, model: &str) -> Result<(), EngineError>;
    /// Returns when the memory is released.
    async fn unload(&self, model: &str) -> Result<(), EngineError>;
    /// Loads each of `models` the engine does not list as ready, and
    /// returns when all of them are. An engine that loads its models
    /// together (one start command) overrides it.
    async fn load_all(&self, models: &[String]) -> Result<(), EngineError> {
        let listed = self.models().await.unwrap_or_default();
        for id in models {
            if !serves(&listed, id) {
                self.load(id).await.map_err(|e| e.about(id))?;
            }
        }
        Ok(())
    }
    /// Unloads each of `models`, and returns when their memory is
    /// released. An engine that frees its models together (one stop
    /// command) overrides it.
    async fn unload_all(&self, models: &[String]) -> Result<(), EngineError> {
        for id in models {
            self.unload(id).await.map_err(|e| e.about(id))?;
        }
        Ok(())
    }
    /// Whether `unload` and a later `load` can work. When they cannot, a
    /// media job or a heavy job runs with the LLM still loaded.
    fn can_unload(&self) -> bool {
        true
    }
}

/// A request to a media engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaJob {
    /// `image`, `video`, `tts`, `stt`, ...
    pub kind: String,
    pub params: serde_json::Value,
    pub output_dir: PathBuf,
}

/// What a media job produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaOutput {
    pub paths: Vec<PathBuf>,
    pub detail: serde_json::Value,
}

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("media engine unreachable: {0}")]
    Unreachable(String),
    #[error("media job failed: {0}")]
    Failed(String),
    #[error("media job timed out after {0:?}")]
    Timeout(Duration),
}

/// An image, video, TTS or STT engine that borrows the memory for one job.
#[async_trait]
pub trait MediaAdapter: Send + Sync {
    fn name(&self) -> &str;
    /// Starts or warms the engine.
    async fn load(&self) -> Result<(), MediaError>;
    async fn run(&self, job: MediaJob) -> Result<MediaOutput, MediaError>;
    /// Frees the engine's models so the LLM can reload.
    async fn unload(&self) -> Result<(), MediaError>;
    /// The media model is served by the LLM's own engine, which manages the
    /// memory of both: the LLM is not unloaded for a job.
    fn shares_engine(&self) -> bool {
        false
    }
}
