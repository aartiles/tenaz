//! What the binary's end-to-end tests share.

use std::path::{Path, PathBuf};
use std::process::Command;

/// An empty home for the binary and the tests' own git, shared by the tests
/// of one binary: nothing they run writes to it.
pub fn home() -> PathBuf {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("home");
    std::fs::create_dir_all(&home).unwrap();
    home
}

/// The binary, kept from this machine: it probes no engines or image
/// servers, and its home is an empty folder instead of the developer's, so
/// their `~/.pi` and other settings do not leak into the test. A test that
/// needs a home with something in it sets `HOME` again.
pub fn tenaz() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tenaz"));
    command.env("TENAZ_NO_PROBE", "1").env("HOME", home());
    command
}

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
