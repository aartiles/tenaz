//! Domain core of the single-model runner: the task loop, its state on disk,
//! the watchdog rules, the lease over one resident engine, and the ports the
//! adapters implement. No process, network or git code lives here.

pub mod backlog;
pub mod briefing;
pub mod doctor;
pub mod effort;
pub mod lease;
pub mod ledger;
pub mod memory;
pub mod metrics;
pub mod orchestrator;
pub mod ownership;
pub mod params;
pub mod ports;
pub mod project;
pub mod role;
pub mod session;
pub mod spec;
pub mod task;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod util;
pub mod verify;
pub mod watchdog;
