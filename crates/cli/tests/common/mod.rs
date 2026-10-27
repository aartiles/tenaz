//! What the binary's end-to-end tests share.

/// `--set` overrides that keep a fake run from waiting or notifying. Memory
/// sampling is off too: on macOS it runs `lsof` and `top` on whatever
/// listens on the engine's port (in the wizard test, a stand-in) at both
/// ends of every session, about a second each.
pub const FAST: [&str; 8] = [
    "--set",
    "engineRetryBaseSec=0",
    "--set",
    "capacityRetryWaitSec=0",
    "--set",
    "notify=false",
    "--set",
    "memorySampleSec=0",
];
