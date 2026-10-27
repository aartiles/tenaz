//! What the binary's end-to-end tests share.

/// `--set` overrides that keep a fake run from waiting or notifying.
pub const FAST: [&str; 6] = [
    "--set",
    "engineRetryBaseSec=0",
    "--set",
    "capacityRetryWaitSec=0",
    "--set",
    "notify=false",
];
