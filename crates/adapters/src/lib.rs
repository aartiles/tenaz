//! Adapters for the runner's ports: the real git, the `verify` process, the
//! system clock, macOS notifications and memory sampling, Prometheus engine
//! metrics, and the engine and media back ends.

pub mod clock;
pub mod engine;
pub mod git;
pub mod http;
pub mod media;
pub mod metrics;
pub mod notify;
pub mod probe_macos;
pub mod sandbox;
pub mod verify;
