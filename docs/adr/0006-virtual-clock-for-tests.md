# ADR 0006: A virtual clock for the orchestrator's tests

Status: accepted, 2026-10-28.

## Context

The orchestrator waits on the `Clock` port (`crates/core/src/ports.rs`):
the 5 s grace after an abort and the kill deadline, the session's wall
clock and a tool call's time limit (ADR 0004), engine retry backoffs, the
capacity wait before a rerun (10 s by default), the wait for the engine,
and the samplers' intervals.

The tests ran these on `FastClock`, the wall clock sped up 100 times.
That has three problems:

- A test still waits in real time, only less: a one-minute tool time limit
  is 0.6 s, played while the scripted harness polls every 5 ms.
- The order of two timers depends on how busy the machine is. A 1 s
  answer to an abort against a 5 s grace is 10 ms against 50 ms; a slow CI
  runner can swap them. `stop_grace` already needed paused time to be
  stable.
- Speed is a number each test picks (`100`, sometimes `1`), and a
  scenario that needs a longer wait has to pick a bigger one.

## Options

### A. Keep `FastClock`

- Nothing to change. The problems above stay, and grow with every timed
  scenario.

### B. A clock on tokio's time, run under paused time

- `VirtualClock` reads `tokio::time::Instant` and sleeps with
  `tokio::time::sleep`, at tokio's own pace. Scenario tests run with
  `#[tokio::test(start_paused = true)]`.
- With time paused, tokio moves its clock to the next timer only when
  every task is idle. A wait takes no real time, and timers fire in the
  order of their deadlines whatever the load.
- Anything else in a scenario that sleeps or polls moves the same
  paused clock, so the scripted harness must wait on events instead.
- It needs tokio's `test-util` feature in the tests (core already has it
  for `stop_grace`).
- Paused time cannot be used with real processes: while tokio waits on a
  child's I/O it is idle, so it jumps to the next timer, and a `verify`
  time limit would fire at once. Tests that run real `git` and `sh` keep
  real time.

### C. A `ManualClock` the test moves by hand

- `sleep` parks until the test calls `advance`. Fully deterministic, and
  independent of tokio.
- A whole run has dozens of waits the test does not know about in
  advance. Each scenario would have to drive time step by step, or the
  clock would need its own way to tell that the run is idle, which is
  what tokio's paused time already does.

## Decision

**B.** `tenaz_core::testing::VirtualClock` replaces `FastClock`. Every
scenario in core runs under paused time. Its wall-clock time is the time
it was made plus tokio's elapsed time, so the journal agrees with the
run's waits.

The scripted harness waits on the runner instead of polling it: a call
that runs until its turn is aborted waits for the abort, and a reply waits
until the runner has handled the events before it. Its events go through
one channel that counts what was sent and what the runner handled
(`testing/paced.rs`); closing the session releases every wait. A poll
would move the paused clock by its interval on every turn, and a long
tool time limit would cost thousands of them.

`crates/adapters/tests/existing_project.rs`, the one run on real `git` and
`sh`, uses `SystemClock` in real time. Its engine and capacity waits are
zero (`engineRetryBaseSec=0`, `capacityRetryWaitSec=0`); the other
recoveries it plays end on the harness's events, not on a timer.

## Consequences

- No scenario depends on how fast the machine is. A timed scenario costs
  what its events cost, not what its waits are.
- A new scenario must use `start_paused = true`. Without it the virtual
  clock runs in real time, so the test is slow and its timers race again.
  `scripts/check-architecture.sh` rejects a `#[tokio::test]` without it
  in a core file that names `VirtualClock`, as every scenario does.
- A test that needs real processes and a timed recovery has no clock that
  fits. None does today.
