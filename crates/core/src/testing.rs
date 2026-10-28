//! Fakes for tests: a scripted harness that exercises the orchestrator
//! without a model (a happy path, each recovery opt-in), a clock on
//! tokio's paused time, replayed memory readings and engine counters, the
//! scripted harness's `verify` judged without a shell, and a repository kept
//! as snapshots in memory with the contract that holds it to git.

mod clock;
mod harness;
mod notifier;
mod pace;
mod player;
mod replay;
mod vcs;
pub mod vcs_contract;
mod verify;

pub use clock::VirtualClock;
pub use harness::ScriptedHarness;
pub use notifier::SilentNotifier;
pub use player::VERIFY;
pub use replay::Replay;
pub use vcs::SnapshotVcs;
pub use vcs_contract::MakeVcs;
pub use verify::ScriptedVerify;
