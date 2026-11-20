//! Adapters for the runner's ports: the real git, the `verify` process, the
//! system clock, macOS notifications and memory sampling, Prometheus engine
//! metrics, the engine and media back ends, and the search for what agent
//! sessions left running; and, for tests, a way to write a script that is
//! then run.

pub mod clock;
pub mod engine;
pub mod git;
pub mod http;
pub mod job;
pub mod media;
pub mod metrics;
pub mod notify;
pub mod probe_macos;
mod process;
pub mod sandbox;
pub mod swept;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod verify;
