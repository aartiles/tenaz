//! Fakes and fixtures for tests: a scripted harness that exercises the
//! orchestrator without a model (a happy path, each recovery opt-in), a
//! clock on tokio's paused time, replayed memory readings and engine
//! counters, the scripted harness's `verify` judged without a shell, a
//! process table read from listings a test gives it, a repository kept as
//! snapshots in memory with the contract that holds it to git, and the
//! templates this build ships.

mod abort;
mod clock;
mod harness;
mod jobs;
mod notifier;
mod paced;
mod player;
mod processes;
mod replay;
mod vcs;
pub mod vcs_contract;
mod verify;

/// Sushi's refusal of a request whose prompt and output limit do not fit
/// its memory, as the engine proxy reports it: session 014 of the Last
/// Delivery run of 2026-11-18.
pub const SUSHI_OUTPUT_LIMIT_REFUSAL: &str = "400: Context 103038 does not fit in GPU memory: the request needs 3.6 GB and 3.4 GB is available. max_tokens 13186 fits (asked 16384); maximum context: 99840 tokens.\nTo serve it:\n- Lower max_tokens to 13186 or less.\n- Shorten the prompt.\n- Quit other apps or unload another model on the Models page, then retry.\n- Use a smaller pack of this model.";

pub use clock::VirtualClock;
pub use harness::{
    Removed, Routing, ScriptedConflict, ScriptedHarness, ScriptedJob, ScriptedPassingConflict,
    ScriptedRemoval, ScriptedUnstartable, UNDECLARED,
};
pub use jobs::ScriptedJobs;
pub use notifier::SilentNotifier;
pub use player::{OUTSIDE, PI_MODEL_NOT_FOUND, VERIFY, fx_context_exceeded_exit};
pub use processes::ScriptedProcesses;
pub use replay::Replay;
pub use vcs::SnapshotVcs;
pub use vcs_contract::MakeVcs;
pub use verify::ScriptedVerify;

/// The templates this build ships, from `crates/cli/templates`.
pub fn shipped_templates() -> crate::orchestrator::Templates {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../cli/templates");
    crate::orchestrator::Templates::load(&dir).expect("the shipped templates load")
}
